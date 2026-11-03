//! What FXServer derives from one resource at start: the stream file list
//! (`resource.sfl` + `.db`) and the client package file list
//! (`resource.rpf` + `.db`), computed with FXServer's exact path strings.

use crate::db::FileDb;
use crate::hashindex::HashIndex;
use crate::sfl::StreamEntry;
use crate::stamp::FileStamp;
use fxs_manifest::Manifest;
use std::path::{Path, PathBuf};

/// Map an FXServer path string back to a disk path. FXServer paths only
/// differ by doubled slashes, which every OS accepts, so this is identity.
pub fn disk(p: &str) -> PathBuf {
    PathBuf::from(p)
}

/// `IterateRecursively(<res>/stream/)`: every file, depth-first, a
/// directory's own files before its sub-directories, sub-directories whose
/// name starts with `.` skipped. Paths look like `<res>/stream//sub/file`.
pub fn stream_files(fx_path: &str) -> Vec<String> {
    let mut files = Vec::new();
    fn recurse(parent: &str, files: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(disk(parent)) else { return };
        let mut dirs = Vec::new();
        for ent in rd.flatten() {
            let name = ent.file_name().to_string_lossy().into_owned();
            let full = format!("{parent}/{name}");
            let is_dir = std::fs::metadata(ent.path()).map(|m| m.is_dir()).unwrap_or(false);
            if is_dir {
                if !name.starts_with('.') {
                    dirs.push(full);
                }
            } else {
                files.push(full);
            }
        }
        for d in dirs {
            recurse(&d, files);
        }
    }
    recurse(&format!("{fx_path}/stream/"), &mut files);
    files
}

/// Stream entries in `resource.sfl` order (byte-sorted by base name, first
/// path wins for duplicate names), plus the `.db` over every file seen.
pub fn stream_set(fx_path: &str, index: &HashIndex) -> (Vec<StreamEntry>, FileDb) {
    let files = stream_files(fx_path);
    let db = FileDb::snapshot(files.iter().cloned(), |f| index.stamp(f));
    let mut by_name: std::collections::BTreeMap<String, StreamEntry> = Default::default();
    for f in &files {
        if f.contains(".stream_raw") {
            continue;
        }
        let Some(entry) = index.stream_entry(f) else { continue };
        by_name.entry(entry.name.clone()).or_insert(entry);
    }
    (by_name.into_values().collect(), db)
}

/// `GetFilesForSet("resource.rpf")`, before de-duplication.
pub fn client_files(m: &Manifest, fx_path: &str) -> Vec<String> {
    let mut v = Vec::new();
    v.push(if m.is_cfxv2 || m.has("is_cfxv2") { "fxmanifest.lua".to_string() } else { "__resource.lua".to_string() });
    for key in ["file", "client_script", "shared_script", "map"] {
        v.extend(m.glob_entries(fx_path, key));
    }
    v.retain(|f| {
        if !f.starts_with("stream/") {
            return true;
        }
        match f.find('.') {
            Some(dot) => {
                let ext = &f[dot..];
                !(ext.len() > 1 && ext.as_bytes()[1] == b'y' && ext != ".ytyp")
            }
            None => true,
        }
    });
    v
}

/// The sorted, de-duplicated file set that goes into `resource.rpf`.
pub fn client_set(m: &Manifest, fx_path: &str) -> Vec<String> {
    let set: std::collections::BTreeSet<String> = client_files(m, fx_path).into_iter().collect();
    set.into_iter().collect()
}

pub fn client_db(fx_path: &str, set: &[String]) -> FileDb {
    FileDb::snapshot(set.iter().map(|f| format!("{fx_path}/{f}")), |p| FileStamp::of(&disk(p)))
}

pub fn build_rpf(fx_path: &str, set: &[String]) -> std::io::Result<Vec<u8>> {
    let mut b = crate::rpf2::PackfileBuilder::new();
    for f in set {
        b.add_file(f, &format!("{fx_path}/{f}"));
    }
    b.build()
}

/// Resources protected by Cfx.re escrow ship a `.fxap` file. Their cache
/// entries are produced by closed code inside FXServer (for example the
/// encrypted flag on stream entries), so we never write caches for them.
pub fn is_escrowed(dir: &Path) -> bool {
    dir.join(".fxap").exists()
}
