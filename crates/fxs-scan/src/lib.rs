//! Resource discovery with FXServer's rules (`ServerResourceList.cpp`):
//! breadth-first from `resources/`, `[name]` folders are categories, any other
//! folder is a resource named after itself, the first copy of a name wins.
//!
//! Manifests are then evaluated in parallel, which FXServer does serially.

pub mod deps;

use fxs_manifest::{Manifest, ManifestError};
use rayon::prelude::*;
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct Resource {
    pub name: String,
    /// The path string exactly as FXServer's `Resource::GetPath()` returns
    /// it, e.g. `Z:/srv/resources//[cat]/name`. FXServer writes paths built
    /// from this into its cache databases, so it must match byte for byte.
    pub fx_path: String,
    pub dir: PathBuf,
    /// `[category]` components between the scan root and the resource.
    pub categories: Vec<String>,
    /// Found under FXServer's `citizen/system_resources` rather than `resources/`.
    pub system: bool,
    pub manifest: Result<Manifest, ManifestError>,
}

#[derive(Debug, Default)]
pub struct ScanReport {
    pub resources: Vec<Resource>,
    pub duplicates: Vec<(String, PathBuf)>,
    pub category_manifests: Vec<PathBuf>,
    pub walk_time: Duration,
    pub manifest_time: Duration,
}

#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// FXServer's server root as it formats it: forward slashes, trailing slash.
    pub fx_root: String,
    pub use_system_chat: bool,
    /// FXServer's `citizen` folder: (path string as FXServer formats it, with
    /// trailing slash; folder on disk). Its `system_resources` are scanned
    /// after `resources/`, as FXServer does.
    pub citizen: Option<(String, PathBuf)>,
}

impl ScanOptions {
    pub fn for_root(root: &Path) -> Self {
        let mut s = root.to_string_lossy().replace('\\', "/");
        if !s.ends_with('/') {
            s.push('/');
        }
        let citizen = root.join("bin").join("citizen");
        let citizen = citizen.join("system_resources").is_dir().then(|| (format!("{s}bin/citizen/"), citizen));
        Self { fx_root: s, use_system_chat: false, citizen }
    }
}

fn is_dir(p: &Path) -> bool {
    std::fs::metadata(p).map(|m| m.is_dir()).unwrap_or(false)
}

pub fn scan(root: &Path, opts: &ScanOptions) -> ScanReport {
    let t0 = Instant::now();
    let mut report = ScanReport::default();
    let mut seen = HashSet::new();
    let mut found: Vec<(String, String, PathBuf, Vec<String>, bool)> = Vec::new();

    // (fx path, disk path, categories, system root). FXServer finishes the
    // user root before it scans system_resources, so user copies win.
    let mut roots = vec![(format!("{}resources/", opts.fx_root), root.join("resources"), false)];
    if let Some((fx, disk)) = &opts.citizen {
        roots.push((format!("{fx}system_resources/"), disk.join("system_resources"), true));
    }
    for (root_fx, root_disk, system) in roots {
    let mut queue: VecDeque<(String, PathBuf, Vec<String>)> = VecDeque::new();
    queue.push_back((root_fx, root_disk, Vec::new()));

    while let Some((fx, disk, cats)) = queue.pop_front() {
        let Ok(rd) = std::fs::read_dir(&disk) else { continue };
        for ent in rd.flatten() {
            let name = ent.file_name().to_string_lossy().into_owned();
            if name == "." || name == ".." || name == ".git" {
                continue;
            }
            // system chat is used only with resources_useSystemChat
            if name == "chat" && opts.use_system_chat != system {
                continue;
            }
            if !system && (name == "yarn" || name == "webpack") {
                continue;
            }
            let path = ent.path();
            if !is_dir(&path) {
                continue;
            }
            let child_fx = format!("{fx}/{name}");
            if name.starts_with('[') && name.ends_with(']') {
                if path.join("fxmanifest.lua").exists() || path.join("__resource.lua").exists() {
                    report.category_manifests.push(path.clone());
                }
                let mut c = cats.clone();
                c.push(name);
                queue.push_back((child_fx, path, c));
                continue;
            }
            if name.starts_with('.') || name.eq_ignore_ascii_case("txadmin") {
                continue;
            }
            if !seen.insert(name.clone()) {
                report.duplicates.push((name, path));
                continue;
            }
            found.push((name, child_fx, path, cats.clone(), system));
        }
    }
    }
    report.walk_time = t0.elapsed();

    let t1 = Instant::now();
    report.resources = found
        .into_par_iter()
        .map(|(name, fx_path, dir, categories, system)| {
            let manifest = fxs_manifest::load(&dir);
            Resource { name, fx_path, dir, categories, system, manifest }
        })
        .collect();
    report.manifest_time = t1.elapsed();
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn categories_and_duplicates() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        for p in ["resources/[a]/one", "resources/[a]/[b]/two", "resources/three", "resources/[c]/three", "resources/.hidden", "resources/[a]/nomani"] {
            fs::create_dir_all(r.join(p)).unwrap();
        }
        for p in ["resources/[a]/one", "resources/[a]/[b]/two", "resources/three", "resources/[c]/three"] {
            fs::write(r.join(p).join("fxmanifest.lua"), "fx_version 'cerulean'\ngame 'gta5'\n").unwrap();
        }
        let opts = ScanOptions { fx_root: "R:/srv/".into(), use_system_chat: false, citizen: None };
        let rep = scan(r, &opts);
        let mut names: Vec<_> = rep.resources.iter().map(|r| r.name.as_str()).collect();
        names.sort();
        assert_eq!(names, ["nomani", "one", "three", "two"]);
        assert_eq!(rep.duplicates.len(), 1);
        let two = rep.resources.iter().find(|r| r.name == "two").unwrap();
        assert_eq!(two.fx_path, "R:/srv/resources//[a]/[b]/two");
        assert_eq!(two.categories, ["[a]", "[b]"]);
        let three = rep.resources.iter().find(|r| r.name == "three").unwrap();
        assert_eq!(three.fx_path, "R:/srv/resources//three", "BFS: top-level copy wins");
        assert!(rep.resources.iter().find(|r| r.name == "nomani").unwrap().manifest.is_err());
    }
}
