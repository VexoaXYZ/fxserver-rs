//! Read-only memory map of another process (Windows): walks the address space
//! with `VirtualQueryEx` and attributes committed memory to image modules,
//! mapped files, and private allocations grouped by allocation base.
//! Opens the process with query + read rights only; never writes or injects.

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Group {
    pub label: String,
    pub committed: u64,
    pub private_writable: u64,
    pub regions: u32,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct MemMap {
    pub committed_total: u64,
    pub private_total: u64,
    pub image_total: u64,
    pub mapped_total: u64,
    pub reserved_total: u64,
    pub images: Vec<Group>,
    pub mapped: Vec<Group>,
    /// Private allocations: (base, committed, reserved-only), largest first.
    pub private_allocs: Vec<(u64, u64, u64)>,
    /// Private commit by allocation size bucket.
    pub private_buckets: Vec<(String, u64, u32)>,
}

#[cfg(windows)]
pub fn memmap(pid: u32) -> Result<MemMap, String> {
    use std::collections::HashMap;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Memory::{
        VirtualQueryEx, MEMORY_BASIC_INFORMATION, MEM_COMMIT, MEM_IMAGE, MEM_MAPPED, MEM_PRIVATE, MEM_RESERVE,
        PAGE_EXECUTE_READWRITE, PAGE_READWRITE,
    };
    use windows_sys::Win32::System::ProcessStatus::GetMappedFileNameW;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ};

    let h = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid) };
    if h.is_null() {
        return Err(format!("OpenProcess({pid}) failed: {}", std::io::Error::last_os_error()));
    }
    let mut out = MemMap::default();
    let mut images: HashMap<String, Group> = HashMap::new();
    let mut mapped: HashMap<String, Group> = HashMap::new();
    let mut privs: HashMap<u64, u64> = HashMap::new();
    let mut priv_res: HashMap<u64, u64> = HashMap::new();
    let mut name_cache: HashMap<u64, String> = HashMap::new();

    let mut addr: usize = 0;
    loop {
        let mut mbi: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        let n = unsafe { VirtualQueryEx(h, addr as *const _, &mut mbi, std::mem::size_of::<MEMORY_BASIC_INFORMATION>()) };
        if n == 0 {
            break;
        }
        let size = mbi.RegionSize as u64;
        let base = mbi.AllocationBase as u64;
        if mbi.State == MEM_RESERVE {
            out.reserved_total += size;
            if mbi.Type == MEM_PRIVATE {
                *priv_res.entry(base).or_default() += size;
            }
        }
        if mbi.State == MEM_COMMIT {
            out.committed_total += size;
            let writable = mbi.Protect & (PAGE_READWRITE | PAGE_EXECUTE_READWRITE) != 0;
            let mut file_name = || {
                name_cache
                    .entry(base)
                    .or_insert_with(|| {
                        let mut buf = [0u16; 1024];
                        let len = unsafe { GetMappedFileNameW(h, base as *const _, buf.as_mut_ptr(), buf.len() as u32) };
                        let full = String::from_utf16_lossy(&buf[..len as usize]);
                        full.rsplit('\\').next().unwrap_or("").to_string()
                    })
                    .clone()
            };
            match mbi.Type {
                MEM_IMAGE => {
                    out.image_total += size;
                    let g = images.entry(file_name()).or_default();
                    g.committed += size;
                    if writable {
                        g.private_writable += size;
                    }
                    g.regions += 1;
                }
                MEM_MAPPED => {
                    out.mapped_total += size;
                    let name = file_name();
                    let g = mapped.entry(if name.is_empty() { "(pagefile-backed section)".into() } else { name }).or_default();
                    g.committed += size;
                    g.regions += 1;
                }
                MEM_PRIVATE => {
                    out.private_total += size;
                    *privs.entry(base).or_default() += size;
                }
                _ => {}
            }
        }
        let next = (mbi.BaseAddress as usize).checked_add(mbi.RegionSize);
        match next {
            Some(n) if n > addr => addr = n,
            _ => break,
        }
    }
    unsafe { CloseHandle(h) };

    let finish = |m: HashMap<String, Group>| {
        let mut v: Vec<Group> = m.into_iter().map(|(k, mut g)| {
            g.label = k;
            g
        }).collect();
        v.sort_by(|a, b| b.committed.cmp(&a.committed));
        v
    };
    out.images = finish(images);
    out.mapped = finish(mapped);
    let mut pa: Vec<(u64, u64, u64)> = privs.into_iter().map(|(b, c)| (b, c, priv_res.get(&b).copied().unwrap_or(0))).collect();
    pa.sort_by(|a, b| b.1.cmp(&a.1));
    let buckets: [(&str, u64); 6] = [("< 64 KiB", 64 << 10), ("64 KiB - 1 MiB", 1 << 20), ("1 - 16 MiB", 16 << 20), ("16 - 64 MiB", 64 << 20), ("64 - 256 MiB", 256 << 20), (">= 256 MiB", u64::MAX)];
    let mut b: Vec<(String, u64, u32)> = buckets.iter().map(|(n, _)| (n.to_string(), 0, 0)).collect();
    for (_, sz, _) in &pa {
        let i = buckets.iter().position(|(_, lim)| sz < lim).unwrap_or(buckets.len() - 1);
        b[i].1 += sz;
        b[i].2 += 1;
    }
    out.private_buckets = b;
    out.private_allocs = pa;
    Ok(out)
}

#[derive(Debug, Default, serde::Serialize)]
pub struct Residency {
    pub private_committed: u64,
    pub private_resident: u64,
    pub heap_segments_committed: u64,
    pub heap_segments_resident: u64,
    pub heap_segments: u32,
    pub other_committed: u64,
    pub other_resident: u64,
    /// (base, committed, resident, is_heap_segment), largest committed first
    pub allocs: Vec<(u64, u64, u64, bool)>,
}

/// Committed vs physically resident private memory, per allocation, using
/// `QueryWorkingSetEx`. NT heap segments are recognised by their
/// `0xffeeffee` segment signature at offset 0x10.
#[cfg(windows)]
pub fn residency(pid: u32) -> Result<Residency, String> {
    use std::collections::HashMap;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows_sys::Win32::System::Memory::{VirtualQueryEx, MEMORY_BASIC_INFORMATION, MEM_COMMIT, MEM_PRIVATE};
    use windows_sys::Win32::System::ProcessStatus::{QueryWorkingSetEx, PSAPI_WORKING_SET_EX_INFORMATION};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ};

    let h = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid) };
    if h.is_null() {
        return Err(format!("OpenProcess({pid}) failed: {}", std::io::Error::last_os_error()));
    }
    let mut regions: Vec<(u64, u64, u64)> = Vec::new(); // (alloc base, addr, size)
    let mut addr: usize = 0;
    loop {
        let mut mbi: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { VirtualQueryEx(h, addr as *const _, &mut mbi, std::mem::size_of::<MEMORY_BASIC_INFORMATION>()) } == 0 {
            break;
        }
        if mbi.State == MEM_COMMIT && mbi.Type == MEM_PRIVATE {
            regions.push((mbi.AllocationBase as u64, mbi.BaseAddress as u64, mbi.RegionSize as u64));
        }
        match (mbi.BaseAddress as usize).checked_add(mbi.RegionSize) {
            Some(n) if n > addr => addr = n,
            _ => break,
        }
    }
    let mut per: HashMap<u64, (u64, u64)> = HashMap::new();
    for &(base, start, size) in &regions {
        let pages = (size / 4096) as usize;
        let mut info: Vec<PSAPI_WORKING_SET_EX_INFORMATION> = (0..pages)
            .map(|i| {
                let mut x: PSAPI_WORKING_SET_EX_INFORMATION = unsafe { std::mem::zeroed() };
                x.VirtualAddress = (start + i as u64 * 4096) as *mut _;
                x
            })
            .collect();
        let mut resident = 0u64;
        for chunk in info.chunks_mut(65536) {
            let ok = unsafe {
                QueryWorkingSetEx(h, chunk.as_mut_ptr() as *mut _, (chunk.len() * std::mem::size_of::<PSAPI_WORKING_SET_EX_INFORMATION>()) as u32)
            };
            if ok != 0 {
                for p in chunk.iter() {
                    // bit 0 of VirtualAttributes = Valid (resident)
                    if unsafe { p.VirtualAttributes.Flags } & 1 != 0 {
                        resident += 4096;
                    }
                }
            }
        }
        let e = per.entry(base).or_default();
        e.0 += size;
        e.1 += resident;
    }
    let mut out = Residency::default();
    for (base, (c, r)) in per {
        let mut sig = [0u8; 4];
        let mut got = 0usize;
        let ok = unsafe { ReadProcessMemory(h, (base + 0x10) as *const _, sig.as_mut_ptr() as *mut _, 4, &mut got) };
        let heap = ok != 0 && got == 4 && u32::from_le_bytes(sig) == 0xffee_ffee;
        out.private_committed += c;
        out.private_resident += r;
        if heap {
            out.heap_segments += 1;
            out.heap_segments_committed += c;
            out.heap_segments_resident += r;
        } else {
            out.other_committed += c;
            out.other_resident += r;
        }
        out.allocs.push((base, c, r, heap));
    }
    out.allocs.sort_by(|a, b| b.1.cmp(&a.1));
    unsafe { CloseHandle(h) };
    Ok(out)
}

#[cfg(not(windows))]
pub fn residency(_pid: u32) -> Result<Residency, String> {
    Err("residency is implemented for Windows; on Linux read /proc/<pid>/smaps".into())
}

/// Read `len` bytes at `addr` from another process (read-only).
#[cfg(windows)]
pub fn peek(pid: u32, addr: u64, len: usize) -> Result<Vec<u8>, String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ};
    let h = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid) };
    if h.is_null() {
        return Err(format!("OpenProcess({pid}) failed: {}", std::io::Error::last_os_error()));
    }
    let mut buf = vec![0u8; len];
    let mut got = 0usize;
    // read page by page so an unreadable page does not fail the whole read
    let mut off = 0usize;
    while off < len {
        let n = (4096 - ((addr as usize + off) % 4096)).min(len - off);
        let mut r = 0usize;
        let ok = unsafe { ReadProcessMemory(h, (addr as usize + off) as *const _, buf[off..].as_mut_ptr() as *mut _, n, &mut r) };
        if ok != 0 {
            got += r;
        }
        off += n;
    }
    unsafe { CloseHandle(h) };
    if got == 0 {
        return Err("nothing readable".into());
    }
    Ok(buf)
}

#[cfg(not(windows))]
pub fn peek(_pid: u32, _addr: u64, _len: usize) -> Result<Vec<u8>, String> {
    Err("peek is implemented for Windows".into())
}

/// Printable ASCII runs of at least `min` bytes, with counts, most common first.
pub fn strings_summary(data: &[u8], min: usize, top: usize) -> Vec<(String, usize)> {
    let mut counts: std::collections::HashMap<String, usize> = Default::default();
    let mut start = None;
    for (i, &b) in data.iter().enumerate() {
        let printable = (0x20..0x7f).contains(&b);
        match (printable, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                if i - s >= min {
                    let s = String::from_utf8_lossy(&data[s..i.min(s + 80)]).into_owned();
                    *counts.entry(s).or_default() += 1;
                }
                start = None;
            }
            _ => {}
        }
    }
    let mut v: Vec<_> = counts.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v.truncate(top);
    v
}

#[cfg(not(windows))]
pub fn memmap(_pid: u32) -> Result<MemMap, String> {
    Err("memmap is implemented for Windows; on Linux read /proc/<pid>/smaps".into())
}
