//! `*.db` files next to FXServer's cached sets (`resource.rpf.db`,
//! `resource.sfl.db`): a compact JSON array of
//! `{"n": path, "mt": mtime, "s": size, "i": base64(file id)}`, sorted by
//! path because FXServer keeps them in a `std::map`.

use crate::stamp::FileStamp;
use base64::Engine;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FileDb {
    pub entries: BTreeMap<String, FileStamp>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Row<'a> {
    #[serde(borrow)]
    n: std::borrow::Cow<'a, str>,
    mt: i64,
    s: u64,
    i: String,
}

impl FileDb {
    pub fn load(path: &Path) -> Option<FileDb> {
        let data = std::fs::read(path).ok()?;
        Self::parse(&data)
    }

    pub fn parse(data: &[u8]) -> Option<FileDb> {
        let rows: Vec<Row> = serde_json::from_slice(data).ok()?;
        let b64 = base64::engine::general_purpose::STANDARD;
        let mut entries = BTreeMap::new();
        for r in rows {
            let raw = b64.decode(r.i.as_bytes()).unwrap_or_default();
            let mut id = [0u8; 16];
            let n = raw.len().min(16);
            id[..n].copy_from_slice(&raw[..n]);
            entries.insert(r.n.into_owned(), FileStamp { id, mtime: r.mt, size: r.s });
        }
        Some(FileDb { entries })
    }

    pub fn encode(&self) -> Vec<u8> {
        let b64 = base64::engine::general_purpose::STANDARD;
        let rows: Vec<Row> = self
            .entries
            .iter()
            .map(|(n, e)| Row { n: n.as_str().into(), mt: e.mtime, s: e.size, i: b64.encode(e.id) })
            .collect();
        serde_json::to_vec(&rows).expect("serialize db")
    }

    /// `ResourceFileDatabase::Snapshot`.
    pub fn snapshot<I: IntoIterator<Item = String>>(files: I, stamp: impl Fn(&str) -> FileStamp) -> FileDb {
        let mut entries = BTreeMap::new();
        for f in files {
            if !entries.contains_key(&f) {
                let s = stamp(&f);
                entries.insert(f, s);
            }
        }
        FileDb { entries }
    }

    /// `ResourceFileDatabase::Check`: true when the set must be rebuilt.
    pub fn is_stale(&self, files: &[String], stamp: impl Fn(&str) -> FileStamp) -> bool {
        let new: std::collections::BTreeSet<&str> = files.iter().map(|s| s.as_str()).collect();
        if new.len() != self.entries.len() {
            return true;
        }
        if self.entries.keys().any(|k| !new.contains(k.as_str())) {
            return true;
        }
        self.entries.iter().any(|(k, v)| stamp(k) != *v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_real_sample() {
        let s = br#"[{"n":"Z:/[FiveM]/[Rivo]/rivo/resources//[rivo]/[lean]/rivo_lean/stream//rivo@lean.ycd","mt":1790549090,"s":2378,"i":"aR6rAAAAEQAAAAAAAAAAAA=="}]"#;
        let db = FileDb::parse(s).unwrap();
        assert_eq!(db.encode(), s.to_vec());
        assert_eq!(FileDb::default().encode(), b"[]");
    }

    #[test]
    fn missing_file_sentinel_matches_fxserver() {
        // as written by FXServer for an `@other/file` client file entry
        let fx = br#"[{"n":"Z:/r/resources//x/@rivo_fx/init.lua","mt":0,"s":18446744073709551615,"i":"AAAAAAAAAAAAAAAAAAAAAA=="}]"#;
        let db = FileDb::snapshot(["Z:/r/resources//x/@rivo_fx/init.lua".to_string()], |p| FileStamp::of(std::path::Path::new(p)));
        assert_eq!(db.encode(), fx.to_vec());
    }
}
