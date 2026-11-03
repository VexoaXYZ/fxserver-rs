//! Pre-build FXServer's per-resource cache (`cache/files/<res>/`) so that,
//! on the next start, FXServer finds every set up to date and skips its
//! single-threaded hashing and packing.
//!
//! The same code path also runs in compare mode, where nothing is written and
//! our output is diffed byte for byte against what FXServer produced.

use crate::db::FileDb;
use crate::hashindex::HashIndex;
use crate::resource;
use fxs_scan::Resource;
use rayon::prelude::*;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Write any set FXServer would rebuild.
    Apply,
    /// Write nothing; compare against FXServer's current cache.
    Compare,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum SetState {
    /// FXServer's cache already matches what we compute.
    Identical,
    /// FXServer's cache is current by its own rules, but bytes differ from ours.
    Differs,
    /// FXServer would rebuild (missing or stale); in Apply mode we wrote it.
    Stale,
    /// Up to date by FXServer's rules; not re-derived (Apply mode).
    Current,
    Skipped,
}

#[derive(Debug, serde::Serialize)]
pub struct ResourceResult {
    pub name: String,
    pub stream: SetState,
    pub rpf: SetState,
    pub stream_files: usize,
    pub client_files: usize,
    pub note: Option<String>,
}

pub struct Warmer<'a> {
    pub cache_files: PathBuf,
    pub index: &'a HashIndex,
    pub mode: Mode,
}

fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let tmp = path.with_extension(format!("{}.fxs-tmp", path.extension().and_then(|e| e.to_str()).unwrap_or("")));
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

impl Warmer<'_> {
    /// Hash every stream file of every resource up front, in parallel.
    pub fn prehash(&self, resources: &[&Resource]) {
        let all: Vec<String> = resources
            .par_iter()
            .flat_map_iter(|r| resource::stream_files(&r.fx_path))
            .collect();
        self.index.refresh(&all);
    }

    pub fn run(&self, resources: &[&Resource]) -> Vec<ResourceResult> {
        self.prehash(resources);
        resources.par_iter().map(|r| self.one(r)).collect()
    }

    fn one(&self, r: &Resource) -> ResourceResult {
        let mut res = ResourceResult {
            name: r.name.clone(),
            stream: SetState::Skipped,
            rpf: SetState::Skipped,
            stream_files: 0,
            client_files: 0,
            note: None,
        };
        if r.system {
            res.note = Some("system resource: left to FXServer".into());
            return res;
        }
        if resource::is_escrowed(&r.dir) {
            res.note = Some("escrowed (.fxap): left to FXServer".into());
            return res;
        }
        let manifest = match &r.manifest {
            Ok(m) => m,
            Err(e) => {
                res.note = Some(format!("manifest: {e}"));
                return res;
            }
        };
        let dir = self.cache_files.join(&r.name);
        match self.stream(r, &dir) {
            Ok((s, n)) => {
                res.stream = s;
                res.stream_files = n;
            }
            Err(e) => res.note = Some(format!("stream: {e}")),
        }
        match self.rpf(r, manifest, &dir) {
            Ok((s, n, why)) => {
                res.rpf = s;
                res.client_files = n;
                if why.is_some() { res.note = why; }
            }
            Err(e) => res.note = Some(format!("rpf: {e}")),
        }
        res
    }

    fn stream(&self, r: &Resource, dir: &Path) -> std::io::Result<(SetState, usize)> {
        let (entries, db) = resource::stream_set(&r.fx_path, self.index);
        let sfl = crate::sfl::encode(&entries);
        let db_bytes = db.encode();
        let sfl_path = dir.join("resource.sfl");
        let db_path = dir.join("resource.sfl.db");

        // FXServer's ShouldUpdateSet: db must load, file set and stamps must
        // match, and every onDiskPath in the existing sfl must still open.
        let fx_current = match (FileDb::load(&db_path), std::fs::read(&sfl_path)) {
            (Some(old_db), Ok(old_sfl)) => {
                let files: Vec<String> = db.entries.keys().cloned().collect();
                !old_db.is_stale(&files, |f| self.index.stamp(f))
                    && crate::sfl::decode(&old_sfl).iter().all(|e| std::fs::File::open(&e.on_disk_path).is_ok())
            }
            _ => false,
        };
        let identical = std::fs::read(&sfl_path).ok().as_deref() == Some(&sfl[..])
            && std::fs::read(&db_path).ok().as_deref() == Some(&db_bytes[..]);

        let state = match (fx_current, identical) {
            (true, true) => SetState::Identical,
            (true, false) => SetState::Differs,
            (false, _) => SetState::Stale,
        };
        if state == SetState::Stale && self.mode == Mode::Apply {
            self.link_stream_cache(dir, &entries)?;
            write_atomic(&sfl_path, &sfl)?;
            write_atomic(&db_path, &db_bytes)?;
        }
        Ok((state, entries.len()))
    }

    /// `MakeSymLink`: `stream_cache/z<sha1>` hard links, a marker file, and
    /// removal of links for hashes no longer present.
    fn link_stream_cache(&self, dir: &Path, entries: &[crate::sfl::StreamEntry]) -> std::io::Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let sc = dir.join("stream_cache");
        std::fs::create_dir_all(&sc)?;
        let marker = sc.join("these files are hardlinks - they do not take up any disk space by themselves.txt");
        if !marker.exists() {
            std::fs::write(&marker, b"")?;
        }
        let wanted: std::collections::HashSet<&str> = entries.iter().map(|e| e.hash.as_str()).collect();
        for e in entries {
            let link = sc.join(format!("z{}", e.hash));
            if !link.exists() {
                let _ = std::fs::hard_link(&e.on_disk_path, &link);
            }
        }
        if let Ok(rd) = std::fs::read_dir(&sc) {
            for ent in rd.flatten() {
                let n = ent.file_name().to_string_lossy().into_owned();
                if n.len() == 41 && n.starts_with('z') && !wanted.contains(&n[1..]) {
                    let _ = std::fs::remove_file(ent.path());
                }
            }
        }
        Ok(())
    }

    fn rpf(&self, r: &Resource, m: &fxs_manifest::Manifest, dir: &Path) -> std::io::Result<(SetState, usize, Option<String>)> {
        let files = resource::client_files(m, &r.fx_path);
        let set = resource::client_set(m, &r.fx_path);
        let rpf_path = dir.join("resource.rpf");
        let db_path = dir.join("resource.rpf.db");

        // FXServer's ShouldBuildSet
        let full: Vec<String> = files.iter().map(|f| format!("{}/{f}", r.fx_path)).collect();
        let old_db = FileDb::load(&db_path);
        let mut reason = None;
        let fx_current = rpf_path.exists()
            && old_db
                .as_ref()
                .map(|db| !db.is_stale(&full, |p| crate::stamp::FileStamp::of(&resource::disk(p))))
                .unwrap_or(false);
        if !fx_current {
            reason = Some(match (&old_db, rpf_path.exists()) {
                (_, false) | (None, _) => "rpf: no cache yet".to_string(),
                (Some(db), true) => {
                    let new: std::collections::BTreeSet<&String> = full.iter().collect();
                    let old: std::collections::BTreeSet<&String> = db.entries.keys().collect();
                    let added: Vec<_> = new.difference(&old).take(3).map(|s| s.rsplit('/').next().unwrap_or(s).to_string()).collect();
                    let removed: Vec<_> = old.difference(&new).take(3).map(|s| s.rsplit('/').next().unwrap_or(s).to_string()).collect();
                    if !added.is_empty() || !removed.is_empty() {
                        format!("rpf: file set changed +{added:?} -{removed:?}")
                    } else {
                        let changed: Vec<_> = db
                            .entries
                            .iter()
                            .filter(|(k, v)| crate::stamp::FileStamp::of(&resource::disk(k)) != **v)
                            .take(3)
                            .map(|(k, _)| k.rsplit('/').next().unwrap_or(k).to_string())
                            .collect();
                        format!("rpf: files modified since FXServer built it {changed:?}")
                    }
                }
            });
        }

        let need_bytes = !fx_current || self.mode == Mode::Compare;
        let state = if need_bytes {
            let db = resource::client_db(&r.fx_path, &set);
            let db_bytes = db.encode();
            let rpf = resource::build_rpf(&r.fx_path, &set)?;
            if fx_current {
                let same = std::fs::read(&rpf_path).ok().as_deref() == Some(&rpf[..])
                    && std::fs::read(&db_path).ok().as_deref() == Some(&db_bytes[..]);
                if same { SetState::Identical } else { SetState::Differs }
            } else {
                if self.mode == Mode::Apply {
                    write_atomic(&rpf_path, &rpf)?;
                    write_atomic(&db_path, &db_bytes)?;
                }
                SetState::Stale
            }
        } else {
            SetState::Current
        };
        Ok((state, set.len(), reason))
    }
}
