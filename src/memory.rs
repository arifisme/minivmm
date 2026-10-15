use std::io::{self, Read, Write};
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
        // SAFETY: anonymous private mapping with no address hint; the result is checked.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
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
        // SAFETY: ptr and len describe a mapping created by `anonymous`.
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

impl GuestMemory {
    pub fn new(layout: &[(GuestAddress, usize)]) -> Result<Self> {
        let mut sorted = layout.to_vec();
        sorted.sort_by_key(|(start, _)| *start);
        let mut regions: Vec<GuestRegion> = Vec::with_capacity(sorted.len());
        for (start, len) in sorted {
            let end = start.0.checked_add(len as u64);
            let overlaps = regions.last().is_some_and(|prev| prev.end() > start.0);
            if len == 0 || end.is_none() || overlaps {
                return Err(MemoryError::InvalidLayout);
            }
            regions.push(GuestRegion {
                start,
                mapping: Mapping::anonymous(len)?,
            });
        }
        if regions.is_empty() {
            return Err(MemoryError::InvalidLayout);
        }
        Ok(Self { regions })
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

    pub fn read_from<R: Read>(&self, addr: GuestAddress, reader: &mut R, len: usize) -> Result<()> {
        self.host_ptr(addr, len)?;
        let mut chunk = vec![0u8; COPY_CHUNK.min(len)];
        let mut done = 0;
        while done < len {
            let n = chunk.len().min(len - done);
            reader.read_exact(&mut chunk[..n])?;
            self.write(GuestAddress(addr.0 + done as u64), &chunk[..n])?;
            done += n;
        }
        Ok(())
    }

    pub fn write_to<W: Write>(&self, addr: GuestAddress, writer: &mut W, len: usize) -> Result<()> {
        self.host_ptr(addr, len)?;
        let mut chunk = vec![0u8; COPY_CHUNK.min(len)];
        let mut done = 0;
        while done < len {
            let n = chunk.len().min(len - done);
            self.read(GuestAddress(addr.0 + done as u64), &mut chunk[..n])?;
            writer.write_all(&chunk[..n])?;
            done += n;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn stream_round_trip() {
        let mem = two_regions();
        let data: Vec<u8> = (0..0x2000u32).map(|i| i as u8).collect();
        mem.read_from(GuestAddress(0x10000), &mut data.as_slice(), data.len())
            .unwrap();
        let mut out = Vec::new();
        mem.write_to(GuestAddress(0x10000), &mut out, 0x2000)
            .unwrap();
        assert_eq!(out, data);
        assert_eq!(mem.total_size(), 0x3000);
    }
}
