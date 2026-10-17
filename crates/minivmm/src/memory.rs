use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::ptr::NonNull;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("guest range {addr:#x}+{len:#x} is not backed by guest memory")]
    OutOfRange { addr: u64, len: usize },
    #[error("invalid guest memory layout")]
    InvalidLayout,
    #[error("failed to map guest memory: {0}")]
    Mmap(io::Error),
    #[error("memory file holds {actual} bytes but the guest has {expected}")]
    FileSize { expected: u64, actual: u64 },
    #[error("guest memory I/O failed: {0}")]
    Io(#[from] io::Error),
}

pub type Result<T> = std::result::Result<T, MemoryError>;

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct GuestAddress(pub u64);

impl GuestAddress {
    pub const fn raw(self) -> u64 {
        self.0
    }

    pub fn checked_add(self, offset: u64) -> Option<Self> {
        self.0.checked_add(offset).map(Self)
    }
}

/// # Safety
/// Implementors must be plain data: every bit pattern is a valid value and
/// the type contains no padding that would leak uninitialized bytes.
pub unsafe trait Pod: Copy + Default + Send + Sync + 'static {
    fn as_bytes(&self) -> &[u8] {
        // SAFETY: Pod guarantees the value is fully initialized plain data.
        unsafe { std::slice::from_raw_parts((self as *const Self).cast(), size_of::<Self>()) }
    }

    fn as_bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: Pod guarantees any byte pattern is a valid value.
        unsafe { std::slice::from_raw_parts_mut((self as *mut Self).cast(), size_of::<Self>()) }
    }
}

// SAFETY: primitive integers are plain data.
unsafe impl Pod for u8 {}
// SAFETY: primitive integers are plain data.
unsafe impl Pod for u16 {}
// SAFETY: primitive integers are plain data.
unsafe impl Pod for u32 {}
// SAFETY: primitive integers are plain data.
unsafe impl Pod for u64 {}

struct Mapping {
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: the mapping is plain anonymous memory owned by this struct; all
// accesses go through raw-pointer copies that tolerate concurrent guest writes.
unsafe impl Send for Mapping {}
// SAFETY: see above.
unsafe impl Sync for Mapping {}

impl Mapping {
    fn anonymous(len: usize) -> Result<Self> {
        Self::map(len, libc::MAP_ANONYMOUS, -1, 0)
    }

    fn private_file(file: &File, offset: u64, len: usize) -> Result<Self> {
        let offset = libc::off_t::try_from(offset).map_err(|_| MemoryError::InvalidLayout)?;
        Self::map(len, 0, file.as_raw_fd(), offset)
    }

    fn map(len: usize, flags: libc::c_int, fd: libc::c_int, offset: libc::off_t) -> Result<Self> {
        // SAFETY: a private mapping with no address hint; the result is checked. Writes to a
        // file-backed private mapping go to copy-on-write pages and never reach the file.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_NORESERVE | flags,
                fd,
                offset,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(MemoryError::Mmap(io::Error::last_os_error()));
        }
        let ptr = NonNull::new(ptr.cast()).ok_or(MemoryError::InvalidLayout)?;
        Ok(Self { ptr, len })
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: ptr and len describe a mapping created by `map`.
        unsafe { libc::munmap(self.ptr.as_ptr().cast(), self.len) };
    }
}

pub struct GuestRegion {
    start: GuestAddress,
    mapping: Mapping,
}

impl GuestRegion {
    pub fn start(&self) -> GuestAddress {
        self.start
    }

    pub fn len(&self) -> usize {
        self.mapping.len
    }

    pub fn is_empty(&self) -> bool {
        self.mapping.len == 0
    }

    pub fn host_address(&self) -> u64 {
        self.mapping.ptr.as_ptr() as u64
    }

    fn end(&self) -> u64 {
        self.start.0 + self.mapping.len as u64
    }

    fn contains(&self, addr: u64, len: usize) -> bool {
        addr >= self.start.0
            && addr
                .checked_add(len as u64)
                .is_some_and(|end| end <= self.end())
    }
}

pub struct GuestMemory {
    regions: Vec<GuestRegion>,
}

const COPY_CHUNK: usize = 1 << 20;
const PAGE_SIZE: usize = 4096;

impl GuestMemory {
    pub fn new(layout: &[(GuestAddress, usize)]) -> Result<Self> {
        Self::build(layout, |_, len| Mapping::anonymous(len))
    }

    /// Maps every region copy-on-write from `file`, which holds the regions back to back in
    /// address order, so guests restored from the same file share the pages they never write.
    pub fn from_file(layout: &[(GuestAddress, usize)], file: &File) -> Result<Self> {
        let expected: u64 = layout.iter().map(|(_, len)| *len as u64).sum();
        let actual = file.metadata()?.len();
        if actual != expected {
            return Err(MemoryError::FileSize { expected, actual });
        }
        Self::build(layout, |offset, len| {
            Mapping::private_file(file, offset, len)
        })
    }

    fn build(
        layout: &[(GuestAddress, usize)],
        mut map: impl FnMut(u64, usize) -> Result<Mapping>,
    ) -> Result<Self> {
        let mut sorted = layout.to_vec();
        sorted.sort_by_key(|(start, _)| *start);
        let mut regions: Vec<GuestRegion> = Vec::with_capacity(sorted.len());
        let mut file_offset = 0u64;
        for (start, len) in sorted {
            let end = start.0.checked_add(len as u64);
            let overlaps = regions.last().is_some_and(|prev| prev.end() > start.0);
            if len == 0 || !len.is_multiple_of(PAGE_SIZE) || end.is_none() || overlaps {
                return Err(MemoryError::InvalidLayout);
            }
            regions.push(GuestRegion {
                start,
                mapping: map(file_offset, len)?,
            });
            file_offset += len as u64;
        }
        if regions.is_empty() {
            return Err(MemoryError::InvalidLayout);
        }
        Ok(Self { regions })
    }

    /// Writes the regions back to back into `file`, leaving holes for pages that are all
    /// zeroes so the file stays small and those pages are never read back from disk.
    pub fn dump_sparse(&self, file: &File) -> Result<()> {
        file.set_len(0)?;
        file.set_len(self.total_size())?;
        let mut chunk = vec![0u8; COPY_CHUNK];
        let mut file_offset = 0u64;
        for region in &self.regions {
            let mut done = 0;
            while done < region.len() {
                let n = COPY_CHUNK.min(region.len() - done);
                self.read(GuestAddress(region.start.0 + done as u64), &mut chunk[..n])?;
                write_nonzero_pages(file, &chunk[..n], file_offset + done as u64)?;
                done += n;
            }
            file_offset += region.len() as u64;
        }
        Ok(())
    }

    pub fn regions(&self) -> &[GuestRegion] {
        &self.regions
    }

    pub fn total_size(&self) -> u64 {
        self.regions.iter().map(|r| r.len() as u64).sum()
    }

    pub fn contains_range(&self, addr: GuestAddress, len: usize) -> bool {
        self.host_ptr(addr, len).is_ok()
    }

    fn host_ptr(&self, addr: GuestAddress, len: usize) -> Result<*mut u8> {
        let region = self
            .regions
            .iter()
            .find(|r| r.contains(addr.0, len))
            .ok_or(MemoryError::OutOfRange { addr: addr.0, len })?;
        let offset = (addr.0 - region.start.0) as usize;
        // SAFETY: `contains` proved offset + len lies within the mapping.
        Ok(unsafe { region.mapping.ptr.as_ptr().add(offset) })
    }

    pub fn read(&self, addr: GuestAddress, buf: &mut [u8]) -> Result<()> {
        let src = self.host_ptr(addr, buf.len())?;
        // SAFETY: src is valid for buf.len() bytes and cannot overlap a Rust-owned buffer.
        unsafe { std::ptr::copy_nonoverlapping(src, buf.as_mut_ptr(), buf.len()) };
        Ok(())
    }

    pub fn write(&self, addr: GuestAddress, buf: &[u8]) -> Result<()> {
        let dst = self.host_ptr(addr, buf.len())?;
        // SAFETY: dst is valid for buf.len() bytes and cannot overlap a Rust-owned buffer.
        unsafe { std::ptr::copy_nonoverlapping(buf.as_ptr(), dst, buf.len()) };
        Ok(())
    }

    pub fn read_obj<T: Pod>(&self, addr: GuestAddress) -> Result<T> {
        let mut value = T::default();
        self.read(addr, value.as_bytes_mut())?;
        Ok(value)
    }

    pub fn write_obj<T: Pod>(&self, addr: GuestAddress, value: &T) -> Result<()> {
        self.write(addr, value.as_bytes())
    }

    pub fn fill(&self, addr: GuestAddress, len: usize, byte: u8) -> Result<()> {
        let dst = self.host_ptr(addr, len)?;
        // SAFETY: dst is valid for len bytes.
        unsafe { std::ptr::write_bytes(dst, byte, len) };
        Ok(())
    }
}

fn is_zero(page: &[u8]) -> bool {
    page.iter().fold(0, |acc, byte| acc | byte) == 0
}

fn write_nonzero_pages(file: &File, data: &[u8], offset: u64) -> io::Result<()> {
    let mut run_start = None;
    for (index, page) in data.chunks(PAGE_SIZE).enumerate() {
        let start = index * PAGE_SIZE;
        match (is_zero(page), run_start) {
            (false, None) => run_start = Some(start),
            (true, Some(run)) => {
                file.write_all_at(&data[run..start], offset + run as u64)?;
                run_start = None;
            }
            _ => {}
        }
    }
    if let Some(run) = run_start {
        file.write_all_at(&data[run..], offset + run as u64)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use vmm_sys_util::tempfile::TempFile;

    use super::*;

    fn two_regions() -> GuestMemory {
        GuestMemory::new(&[(GuestAddress(0), 0x1000), (GuestAddress(0x10000), 0x2000)]).unwrap()
    }

    #[test]
    fn rejects_overlapping_and_empty_layouts() {
        assert!(GuestMemory::new(&[]).is_err());
        assert!(GuestMemory::new(&[(GuestAddress(0), 0)]).is_err());
        assert!(
            GuestMemory::new(&[(GuestAddress(0), 0x2000), (GuestAddress(0x1000), 0x1000)]).is_err()
        );
        assert!(GuestMemory::new(&[(GuestAddress(u64::MAX - 10), 0x1000)]).is_err());
        assert!(GuestMemory::new(&[(GuestAddress(0), 0x1800)]).is_err());
    }

    #[test]
    fn sparse_dump_maps_back_copy_on_write() {
        let mem = GuestMemory::new(&[(GuestAddress(0), 0x40000), (GuestAddress(0x100000), 0x2000)])
            .unwrap();
        mem.write(GuestAddress(0x3000), b"low").unwrap();
        mem.write(GuestAddress(0x101ffd), b"top").unwrap();
        let file = TempFile::new().unwrap().into_file();
        mem.dump_sparse(&file).unwrap();
        assert_eq!(file.metadata().unwrap().len(), 0x42000);
        assert!(file.metadata().unwrap().blocks() * 512 < 0x42000);

        let layout = [(GuestAddress(0x100000), 0x2000), (GuestAddress(0), 0x40000)];
        let first = GuestMemory::from_file(&layout, &file).unwrap();
        let second = GuestMemory::from_file(&layout, &file).unwrap();
        let mut word = [0u8; 3];
        first.read(GuestAddress(0x101ffd), &mut word).unwrap();
        assert_eq!(&word, b"top");
        first.write(GuestAddress(0x3000), b"one").unwrap();
        second.read(GuestAddress(0x3000), &mut word).unwrap();
        assert_eq!(&word, b"low");
        let mut on_disk = [0u8; 3];
        file.read_exact_at(&mut on_disk, 0x3000).unwrap();
        assert_eq!(&on_disk, b"low");
    }

    #[test]
    fn memory_file_must_match_the_layout() {
        let file = TempFile::new().unwrap().into_file();
        file.set_len(0x1000).unwrap();
        assert!(matches!(
            GuestMemory::from_file(&[(GuestAddress(0), 0x2000)], &file),
            Err(MemoryError::FileSize {
                expected: 0x2000,
                actual: 0x1000
            })
        ));
    }

    #[test]
    fn object_round_trip() {
        let mem = two_regions();
        mem.write_obj(GuestAddress(0x10ff8), &0x1122_3344_5566_7788u64)
            .unwrap();
        assert_eq!(
            mem.read_obj::<u64>(GuestAddress(0x10ff8)).unwrap(),
            0x1122_3344_5566_7788
        );
        assert_eq!(mem.read_obj::<u8>(GuestAddress(0x10ff8)).unwrap(), 0x88);
    }

    #[test]
    fn accesses_outside_regions_fail() {
        let mem = two_regions();
        assert!(mem.read_obj::<u32>(GuestAddress(0xffe)).is_err());
        assert!(mem.read_obj::<u8>(GuestAddress(0x1000)).is_err());
        assert!(mem.write(GuestAddress(0x11ff0), &[0; 0x20]).is_err());
        assert!(mem.read_obj::<u64>(GuestAddress(u64::MAX - 3)).is_err());
        assert!(mem.read_obj::<u8>(GuestAddress(0x12000)).is_err());
        assert!(mem.read_obj::<u8>(GuestAddress(0x11fff)).is_ok());
    }
}
