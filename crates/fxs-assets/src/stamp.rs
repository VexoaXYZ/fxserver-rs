//! File identity as FXServer's `ResourceFileDatabase::GetEntry` records it:
//! 128-bit file id (Windows `FILE_ID_INFO.FileId`, POSIX `st_ino`), mtime in
//! whole Unix seconds, and length. A file that cannot be opened is all zeros.

use std::fs::File;
use std::path::Path;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct FileStamp {
    pub id: [u8; 16],
    pub mtime: i64,
    pub size: u64,
}

impl FileStamp {
    pub fn of(path: &Path) -> FileStamp {
        let Ok(f) = File::open(path) else {
            return FileStamp::default();
        };
        let Ok(meta) = f.metadata() else {
            return FileStamp::default();
        };
        if meta.is_dir() {
            return FileStamp::default();
        }
        FileStamp { id: file_id(&f, &meta), mtime: mtime(&meta), size: meta.len() }
    }
}

#[cfg(windows)]
fn file_id(f: &File, _meta: &std::fs::Metadata) -> [u8; 16] {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{FileIdInfo, GetFileInformationByHandleEx, FILE_ID_INFO};
    let mut info: FILE_ID_INFO = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        GetFileInformationByHandleEx(
            f.as_raw_handle() as _,
            FileIdInfo,
            &mut info as *mut _ as *mut _,
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if ok == 0 {
        return [0; 16];
    }
    info.FileId.Identifier
}

#[cfg(unix)]
fn file_id(_f: &File, meta: &std::fs::Metadata) -> [u8; 16] {
    use std::os::unix::fs::MetadataExt;
    let mut id = [0u8; 16];
    id[..8].copy_from_slice(&meta.ino().to_ne_bytes());
    id
}

#[cfg(windows)]
fn mtime(meta: &std::fs::Metadata) -> i64 {
    use std::os::windows::fs::MetadataExt;
    // FILETIME / 1e7 - 11644473600, unsigned integer math as in FXServer
    (meta.last_write_time() / 10_000_000).wrapping_sub(11_644_473_600) as i64
}

#[cfg(unix)]
fn mtime(meta: &std::fs::Metadata) -> i64 {
    use std::os::unix::fs::MetadataExt;
    meta.mtime()
}
