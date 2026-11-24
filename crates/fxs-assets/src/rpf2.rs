//! RPF2 client package writer, matching FXServer's `PackfileBuilder`.
//!
//! Header (little-endian u32): `RPF2`, tocSize, numEntries, 0, 0 (no
//! encryption), padded to 2048. Entries start at 2048, 16 bytes each:
//! directory `{nameOff, count, firstChild|0x80000000, count}` and file
//! `{nameOff, len, absOffset, len}`. Children of a directory are contiguous
//! and byte-sorted (the client binary-searches them), names follow in
//! pre-order, file data follows in insertion order, each padded to 2048.

use std::fs::File;
use std::io::{self, Read, Write};

struct Node {
    name: String,
    dir: bool,
    backing: String,
    children: Vec<usize>,
}

pub struct PackfileBuilder {
    nodes: Vec<Node>,
}

impl Default for PackfileBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl PackfileBuilder {
    pub fn new() -> Self {
        Self {
            nodes: vec![Node { name: String::new(), dir: true, backing: String::new(), children: vec![] }],
        }
    }

    /// Add `file_name` (relative, `/` separated) backed by `backing`.
    /// Unreadable backing files are skipped, as FXServer does.
    pub fn add_file(&mut self, file_name: &str, backing: &str) {
        if File::open(backing).is_err() {
            return;
        }
        match file_name.find(['/', '\\']) {
            None => {
                let id = self.push(file_name, false, backing);
                self.nodes[0].children.push(id);
            }
            Some(_) => {
                if let Some(dir) = self.find_directory(0, file_name, 0) {
                    let base = &file_name[file_name.rfind(['/', '\\']).unwrap() + 1..];
                    let id = self.push(base, false, backing);
                    self.nodes[dir].children.push(id);
                }
            }
        }
    }

    fn push(&mut self, name: &str, dir: bool, backing: &str) -> usize {
        self.nodes.push(Node { name: name.into(), dir, backing: backing.into(), children: vec![] });
        self.nodes.len() - 1
    }

    fn find_directory(&mut self, at: usize, sv: &str, pos: usize) -> Option<usize> {
        let find = |from: usize| sv.get(from..).and_then(|s| s.find(['/', '\\'])).map(|i| i + from);
        let initial = find(pos)?;
        let mut new_pos = initial;
        // skip blanks ("a//b")
        let next = sv.as_bytes().get(initial + 1).copied();
        if next == Some(b'/') || next == Some(b'\\') {
            new_pos = find(initial + 1).unwrap_or(sv.len());
        }
        let seg = &sv[pos..initial];
        let existing = self.nodes[at].children.iter().copied().find(|&c| self.nodes[c].name == seg);
        let entry = match existing {
            Some(e) if !self.nodes[e].dir => return None,
            Some(e) => e,
            None => {
                let id = self.push(seg, true, "");
                self.nodes[at].children.push(id);
                id
            }
        };
        if find(new_pos + 1).is_some() {
            self.find_directory(entry, sv, new_pos + 1)
        } else {
            Some(entry)
        }
    }

    fn sorted_children(&self, n: usize) -> Vec<usize> {
        let mut c = self.nodes[n].children.clone();
        c.sort_by(|a, b| self.nodes[*a].name.as_bytes().cmp(self.nodes[*b].name.as_bytes()));
        c
    }

    pub fn write_to(&self, out: &mut impl Write) -> io::Result<()> {
        // Entry index assignment, mirroring Write/WriteSubEntries.
        let n = self.nodes.len();
        let mut index = vec![0u32; n];
        let mut first_child = vec![0u32; n];
        let mut order: Vec<usize> = vec![0];
        index[0] = 0;
        fn sub(b: &PackfileBuilder, node: usize, order: &mut Vec<usize>, index: &mut [u32], first: &mut [u32]) {
            if b.nodes[node].dir {
                first[node] = order.len() as u32;
            }
            let kids = b.sorted_children(node);
            for &k in &kids {
                index[k] = order.len() as u32;
                order.push(k);
            }
            for &k in &kids {
                sub(b, k, order, index, first);
            }
        }
        sub(self, 0, &mut order, &mut index, &mut first_child);

        // Names, pre-order over sorted children.
        let mut names: Vec<u8> = Vec::new();
        let mut name_off = vec![0u32; n];
        fn names_of(b: &PackfileBuilder, node: usize, names: &mut Vec<u8>, off: &mut [u32]) {
            if node == 0 {
                off[0] = 0;
                names.extend_from_slice(b"/\0");
            } else {
                off[node] = names.len() as u32;
                names.extend_from_slice(b.nodes[node].name.as_bytes());
                names.push(0);
            }
            for k in b.sorted_children(node) {
                names_of(b, k, names, off);
            }
        }
        names_of(self, 0, &mut names, &mut name_off);

        let align = |x: usize| (x + 2047) & !2047;
        let toc_end = align(2048 + order.len() * 16 + names.len());

        // File data placement, insertion order (WriteFiles walks m_subEntries unsorted).
        let mut file_order = Vec::new();
        fn files_of(b: &PackfileBuilder, node: usize, out: &mut Vec<usize>) {
            if !b.nodes[node].dir {
                out.push(node);
            }
            for &k in &b.nodes[node].children {
                files_of(b, k, out);
            }
        }
        files_of(self, 0, &mut file_order);
        let mut data_off = vec![0u32; n];
        let mut data_len = vec![0u32; n];
        let mut pos = toc_end;
        let mut blobs = Vec::with_capacity(file_order.len());
        for &f in &file_order {
            let mut data = Vec::new();
            File::open(&self.nodes[f].backing)?.read_to_end(&mut data)?;
            data_off[f] = pos as u32;
            data_len[f] = data.len() as u32;
            pos = align(pos + data.len());
            blobs.push(data);
        }

        let mut buf: Vec<u8> = Vec::with_capacity(pos);
        let w = |buf: &mut Vec<u8>, v: u32| buf.extend_from_slice(&v.to_le_bytes());
        w(&mut buf, 0x3246_5052);
        w(&mut buf, toc_end as u32);
        w(&mut buf, order.len() as u32);
        w(&mut buf, 0);
        w(&mut buf, 0);
        buf.resize(2048, 0);
        for &e in &order {
            let node = &self.nodes[e];
            w(&mut buf, name_off[e]);
            if node.dir {
                let c = node.children.len() as u32;
                w(&mut buf, c);
                w(&mut buf, first_child[e] | 0x8000_0000);
                w(&mut buf, c);
            } else {
                w(&mut buf, data_len[e]);
                w(&mut buf, data_off[e]);
                w(&mut buf, data_len[e]);
            }
        }
        buf.extend_from_slice(&names);
        buf.resize(toc_end, 0);
        for data in &blobs {
            buf.extend_from_slice(data);
            let a = align(buf.len());
            buf.resize(a, 0);
        }
        out.write_all(&buf)
    }

    pub fn build(&self) -> io::Result<Vec<u8>> {
        let mut v = Vec::new();
        self.write_to(&mut v)?;
        Ok(v)
    }
}

/// Minimal reader used for verification: lists `(path, offset, len)`.
pub fn list(data: &[u8]) -> Option<Vec<(String, u32, u32)>> {
    let u = |o: usize| data.get(o..o + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
    if u(0)? != 0x3246_5052 {
        return None;
    }
    let count = u(8)? as usize;
    let names_base = 2048 + count * 16;
    let name = |off: u32| -> String {
        let s = &data[names_base + off as usize..];
        let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
        String::from_utf8_lossy(&s[..end]).into_owned()
    };
    let mut out = Vec::new();
    fn walk(i: usize, prefix: &str, data: &[u8], name: &dyn Fn(u32) -> String, out: &mut Vec<(String, u32, u32)>) -> Option<()> {
        let u = |o: usize| data.get(o..o + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
        let base = 2048 + i * 16;
        let (no, a, b) = (u(base)?, u(base + 4)?, u(base + 8)?);
        if b & 0x8000_0000 != 0 {
            let first = (b & 0x7FFF_FFFF) as usize;
            let p = if i == 0 { String::new() } else { format!("{prefix}{}/", name(no)) };
            for c in first..first + a as usize {
                walk(c, &p, data, name, out)?;
            }
        } else {
            out.push((format!("{prefix}{}", name(no)), b, a));
        }
        Some(())
    }
    walk(0, "", data, &name, &mut out)?;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout() {
        let d = tempfile::tempdir().unwrap();
        let mk = |rel: &str, body: &[u8]| {
            let p = d.path().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, body).unwrap();
            p.to_string_lossy().into_owned()
        };
        let mut b = PackfileBuilder::new();
        for (rel, body) in [("a-b/x.lua", &b"1"[..]), ("a/y.lua", b"22"), ("fxmanifest.lua", b"333")] {
            let p = mk(rel, body);
            b.add_file(rel, &p);
        }
        b.add_file("missing.lua", "/definitely/not/here");
        let out = b.build().unwrap();
        assert_eq!(&out[0..4], b"RPF2");
        assert_eq!(out.len() % 2048, 0);
        let mut l = list(&out).unwrap();
        l.sort();
        let names: Vec<_> = l.iter().map(|x| x.0.as_str()).collect();
        assert_eq!(names, ["a-b/x.lua", "a/y.lua", "fxmanifest.lua"]);
        for (n, off, len) in l {
            let body = &out[off as usize..(off + len) as usize];
            assert_eq!(body.len(), match n.as_str() { "a-b/x.lua" => 1, "a/y.lua" => 2, _ => 3 });
        }
    }
}
