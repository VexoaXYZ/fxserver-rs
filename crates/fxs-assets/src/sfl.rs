//! `resource.sfl`: FXServer dumps its `Entry[]` straight to disk. Layout
//! (792 bytes, verified against real caches):
//!
//! ```text
//!   0  char entryName[128]
//! 128  char hashString[128]   40 lowercase hex chars
//! 256  char onDiskPath[512]   absolute FXServer-style path
//! 768  u32  rscPagesVirtual
//! 772  u32  rscPagesPhysical
//! 776  u32  rscFlags          == size in the open-source server
//! 780  u32  rscVersion
//! 784  u32  size
//! 788  u8   isResource
//! 789  u8   e (encrypted)
//! 790  2 bytes padding
//! ```

pub const ENTRY_SIZE: usize = 792;

#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct StreamEntry {
    pub name: String,
    pub hash: String,
    pub on_disk_path: String,
    pub rsc_pages_virtual: u32,
    pub rsc_pages_physical: u32,
    pub rsc_flags: u32,
    pub rsc_version: u32,
    pub size: u32,
    pub is_resource: bool,
    pub encrypted: bool,
}

fn put_str(buf: &mut [u8], s: &str) {
    // strncpy semantics into a zeroed field
    let b = s.as_bytes();
    let n = b.len().min(buf.len());
    buf[..n].copy_from_slice(&b[..n]);
}

fn get_str(buf: &[u8]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

impl StreamEntry {
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        let mut e = [0u8; ENTRY_SIZE];
        put_str(&mut e[0..128], &self.name);
        put_str(&mut e[128..256], &self.hash);
        put_str(&mut e[256..768], &self.on_disk_path);
        e[768..772].copy_from_slice(&self.rsc_pages_virtual.to_le_bytes());
        e[772..776].copy_from_slice(&self.rsc_pages_physical.to_le_bytes());
        e[776..780].copy_from_slice(&self.rsc_flags.to_le_bytes());
        e[780..784].copy_from_slice(&self.rsc_version.to_le_bytes());
        e[784..788].copy_from_slice(&self.size.to_le_bytes());
        e[788] = self.is_resource as u8;
        e[789] = self.encrypted as u8;
        out.extend_from_slice(&e);
    }

    pub fn decode(e: &[u8]) -> StreamEntry {
        StreamEntry {
            name: get_str(&e[0..128]),
            hash: get_str(&e[128..256]),
            on_disk_path: get_str(&e[256..768]),
            rsc_pages_virtual: u32_at(e, 768),
            rsc_pages_physical: u32_at(e, 772),
            rsc_flags: u32_at(e, 776),
            rsc_version: u32_at(e, 780),
            size: u32_at(e, 784),
            is_resource: e[788] != 0,
            encrypted: e[789] != 0,
        }
    }
}

pub fn encode(entries: &[StreamEntry]) -> Vec<u8> {
    let mut out = Vec::with_capacity(entries.len() * ENTRY_SIZE);
    for e in entries {
        e.encode_into(&mut out);
    }
    out
}

pub fn decode(data: &[u8]) -> Vec<StreamEntry> {
    data.chunks_exact(ENTRY_SIZE).map(StreamEntry::decode).collect()
}

/// Decode the RSC7 page flags into a byte size (`ConvertRSC7Size`).
pub fn rsc7_size(flags: u32) -> u32 {
    let pages = ((flags >> 27) & 0x1)
        + (((flags >> 26) & 0x1) << 1)
        + (((flags >> 25) & 0x1) << 2)
        + (((flags >> 24) & 0x1) << 3)
        + (((flags >> 17) & 0x7F) << 4)
        + (((flags >> 11) & 0x3F) << 5)
        + (((flags >> 7) & 0xF) << 6)
        + (((flags >> 5) & 0x3) << 7)
        + (((flags >> 4) & 0x1) << 8);
    let base = 0x200u32.wrapping_shl(flags & 0xF);
    base.wrapping_mul(pages)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let e = StreamEntry {
            name: "rivo@lean.ycd".into(),
            hash: "0674d08a2cd760a87cf57209b90f9c88b7bc4f68".into(),
            on_disk_path: "Z:/x/stream//rivo@lean.ycd".into(),
            rsc_pages_virtual: 0x2004_0000,
            rsc_pages_physical: 0xE000_0000,
            rsc_flags: 2378,
            rsc_version: 46,
            size: 2378,
            is_resource: true,
            encrypted: false,
        };
        let b = encode(std::slice::from_ref(&e));
        assert_eq!(b.len(), ENTRY_SIZE);
        assert_eq!(&b[768..792], &[0, 0, 4, 0x20, 0, 0, 0, 0xe0, 0x4a, 9, 0, 0, 0x2e, 0, 0, 0, 0x4a, 9, 0, 0, 1, 0, 0, 0]);
        assert_eq!(decode(&b), vec![e]);
    }
}
