//! Persistent content index: path -> (stamp, sha1, RSC header). FXServer
//! re-hashes every stream file of a resource when any one changes; we only
//! hash files whose (file id, mtime, size) changed, and we hash them in
//! parallel across every resource at once.

use crate::sfl::StreamEntry;
use crate::stamp::FileStamp;
use rayon::prelude::*;
use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::sync::RwLock;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Indexed {
    pub stamp: FileStamp,
    pub hash: String,
    pub size: u32,
    pub rsc: Option<(u32, u32, u32)>, // (version, virt, phys)
}

#[derive(Default)]
pub struct HashIndex {
    map: RwLock<HashMap<String, Indexed>>,
    stamps: RwLock<HashMap<String, FileStamp>>,
    pub hashed_files: std::sync::atomic::AtomicU64,
    pub hashed_bytes: std::sync::atomic::AtomicU64,
    pub reused_files: std::sync::atomic::AtomicU64,
}

pub fn sha1_file(path: &Path) -> std::io::Result<(String, u64)> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha1::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        h.update(&buf[..n]);
    }
    Ok((hex::encode(h.finalize()), total))
}

fn read_header(path: &Path) -> Option<(u32, u32, u32)> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut hdr = [0u8; 16];
    let mut got = 0;
    while got < 16 {
        match f.read(&mut hdr[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(_) => return None,
        }
    }
    let u = |o: usize| u32::from_le_bytes(hdr[o..o + 4].try_into().unwrap());
    let (magic, version, virt, phys) = (u(0), u(4), u(8), u(12));
    match magic {
        0x3743_5352 | 0x3843_5352 => Some((version, virt, phys)),
        // RSC\x05: FXServer stores the version as the physical page flags
        0x0543_5352 => Some((version, virt, version)),
        _ => None,
    }
}

impl HashIndex {
    pub fn load(path: &Path) -> HashIndex {
        let map: HashMap<String, Indexed> = std::fs::read(path)
            .ok()
            .and_then(|d| serde_json::from_slice(&d).ok())
            .unwrap_or_default();
        HashIndex { map: RwLock::new(map), ..Default::default() }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec(&*self.map.read().unwrap())?)?;
        std::fs::rename(tmp, path)
    }

    /// Seed from FXServer's own `resource.sfl` + `.sfl.db` pairs, so the very
    /// first run reuses every hash FXServer already computed.
    pub fn seed_from_fxserver_cache(&self, cache_files: &Path) -> usize {
        let Ok(rd) = std::fs::read_dir(cache_files) else { return 0 };
        let mut n = 0;
        for ent in rd.flatten() {
            let dir = ent.path();
            let (Ok(sfl), Some(db)) = (std::fs::read(dir.join("resource.sfl")), crate::db::FileDb::load(&dir.join("resource.sfl.db"))) else {
                continue;
            };
            let mut map = self.map.write().unwrap();
            for e in crate::sfl::decode(&sfl) {
                let Some(stamp) = db.entries.get(&e.on_disk_path) else { continue };
                if map.contains_key(&e.on_disk_path) {
                    continue;
                }
                let rsc = e.is_resource.then_some((e.rsc_version, e.rsc_pages_virtual, e.rsc_pages_physical));
                map.insert(e.on_disk_path.clone(), Indexed { stamp: *stamp, hash: e.hash.clone(), size: e.size, rsc });
                n += 1;
            }
        }
        n
    }

    pub fn stamp(&self, fx_path: &str) -> FileStamp {
        if let Some(s) = self.stamps.read().unwrap().get(fx_path) {
            return *s;
        }
        let s = FileStamp::of(Path::new(fx_path));
        self.stamps.write().unwrap().insert(fx_path.to_string(), s);
        s
    }

    /// Hash (or reuse) every path, in parallel.
    pub fn refresh(&self, paths: &[String]) {
        use std::sync::atomic::Ordering::Relaxed;
        paths.par_iter().for_each(|p| {
            if p.contains(".stream_raw") {
                return;
            }
            let stamp = self.stamp(p);
            if stamp == FileStamp::MISSING {
                return;
            }
            if let Some(i) = self.map.read().unwrap().get(p) {
                if i.stamp == stamp {
                    self.reused_files.fetch_add(1, Relaxed);
                    return;
                }
            }
            let Ok((hash, bytes)) = sha1_file(Path::new(p)) else { return };
            let raw = format!("{p}.stream_raw");
            let rsc = if Path::new(&raw).exists() { read_header(Path::new(&raw)) } else { read_header(Path::new(p)) };
            self.hashed_files.fetch_add(1, Relaxed);
            self.hashed_bytes.fetch_add(bytes, Relaxed);
            self.map.write().unwrap().insert(p.clone(), Indexed { stamp, hash, size: stamp.size as u32, rsc });
        });
    }

    pub fn stream_entry(&self, fx_path: &str) -> Option<StreamEntry> {
        let map = self.map.read().unwrap();
        let i = map.get(fx_path)?;
        let name = fx_path[fx_path.rfind(['/', '\\']).map(|x| x + 1).unwrap_or(0)..].to_string();
        let (rsc_version, virt, phys, is_resource) = match i.rsc {
            Some((v, vi, ph)) => (v, vi, ph, true),
            None => (0, 0, 0, false),
        };
        Some(StreamEntry {
            name,
            hash: i.hash.clone(),
            on_disk_path: fx_path.to_string(),
            rsc_pages_virtual: virt,
            rsc_pages_physical: phys,
            rsc_flags: i.size,
            rsc_version,
            size: i.size,
            is_resource,
            encrypted: false,
        })
    }
}
