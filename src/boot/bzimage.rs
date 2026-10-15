use thiserror::Error;

use crate::arch::layout::KERNEL_START;
use crate::memory::{GuestAddress, GuestMemory, MemoryError};

pub const SETUP_HEADER_START: usize = 0x1f1;
const SETUP_HEADER_MAX_END: usize = 0x280;
const OFFSET_SETUP_SECTS: usize = 0x1f1;
const OFFSET_BOOT_FLAG: usize = 0x1fe;
const OFFSET_JUMP_TARGET: usize = 0x201;
const OFFSET_HEADER_MAGIC: usize = 0x202;
const OFFSET_VERSION: usize = 0x206;
const OFFSET_LOADFLAGS: usize = 0x211;
const OFFSET_INITRD_ADDR_MAX: usize = 0x22c;
const OFFSET_XLOADFLAGS: usize = 0x236;
const OFFSET_CMDLINE_SIZE: usize = 0x238;
const OFFSET_INIT_SIZE: usize = 0x260;

const BOOT_FLAG: u16 = 0xaa55;
const HEADER_MAGIC: &[u8; 4] = b"HdrS";
const MIN_BOOT_PROTOCOL: u16 = 0x020c;
const LOADED_HIGH: u8 = 1 << 0;
const XLF_KERNEL_64: u16 = 1 << 0;
const DEFAULT_SETUP_SECTS: usize = 4;
const SECTOR_SIZE: usize = 512;
const STARTUP_64_OFFSET: u64 = 0x200;

#[derive(Debug, Error)]
pub enum BootError {
    #[error("kernel image is too small to contain a setup header")]
    TooSmall,
    #[error("kernel image is not a bzImage (missing boot flag or HdrS magic)")]
    BadMagic,
    #[error("boot protocol {0:#06x} is older than the required 2.12")]
    UnsupportedProtocol(u16),
    #[error("kernel does not support the 64-bit boot entry point")]
    No64BitEntry,
    #[error("kernel is not a bzImage (not loaded high)")]
    NotLoadedHigh,
    #[error("kernel needs {needed:#x} bytes but only {available:#x} fit below the MMIO gap")]
    KernelTooLarge { needed: u64, available: u64 },
    #[error("command line is {len} bytes but the kernel accepts at most {max}")]
    CmdlineTooLong { len: usize, max: usize },
    #[error("command line contains a NUL byte")]
    CmdlineHasNul,
    #[error("initrd of {0:#x} bytes does not fit in guest memory")]
    InitrdTooLarge(usize),
    #[error(transparent)]
    Memory(#[from] MemoryError),
}

pub type Result<T> = std::result::Result<T, BootError>;

#[derive(Clone, Debug)]
pub struct KernelHeader {
    raw: Vec<u8>,
    pub protocol: u16,
    pub cmdline_size: usize,
    pub initrd_addr_max: u64,
    pub init_size: u64,
}

impl KernelHeader {
    pub fn raw(&self) -> &[u8] {
        &self.raw
    }
}

#[derive(Clone, Debug)]
pub struct LoadedKernel {
    pub entry: GuestAddress,
    pub end: GuestAddress,
    pub header: KernelHeader,
}

fn u16_at(image: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([image[offset], image[offset + 1]])
}

fn u32_at(image: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(image[offset..offset + 4].try_into().unwrap_or_default())
}

pub fn parse_header(image: &[u8]) -> Result<KernelHeader> {
    if image.len() < SETUP_HEADER_MAX_END {
        return Err(BootError::TooSmall);
    }
    if u16_at(image, OFFSET_BOOT_FLAG) != BOOT_FLAG
        || &image[OFFSET_HEADER_MAGIC..OFFSET_HEADER_MAGIC + 4] != HEADER_MAGIC
    {
        return Err(BootError::BadMagic);
    }
    let protocol = u16_at(image, OFFSET_VERSION);
    if protocol < MIN_BOOT_PROTOCOL {
        return Err(BootError::UnsupportedProtocol(protocol));
    }
    if image[OFFSET_LOADFLAGS] & LOADED_HIGH == 0 {
        return Err(BootError::NotLoadedHigh);
    }
    if u16_at(image, OFFSET_XLOADFLAGS) & XLF_KERNEL_64 == 0 {
        return Err(BootError::No64BitEntry);
    }
    let header_end = (OFFSET_HEADER_MAGIC + usize::from(image[OFFSET_JUMP_TARGET]))
        .clamp(OFFSET_INIT_SIZE + 4, SETUP_HEADER_MAX_END);
    Ok(KernelHeader {
        raw: image[SETUP_HEADER_START..header_end].to_vec(),
        protocol,
        cmdline_size: u32_at(image, OFFSET_CMDLINE_SIZE) as usize,
        initrd_addr_max: u64::from(u32_at(image, OFFSET_INITRD_ADDR_MAX)),
        init_size: u64::from(u32_at(image, OFFSET_INIT_SIZE)),
    })
}

fn setup_size(image: &[u8]) -> usize {
    let sects = match usize::from(image[OFFSET_SETUP_SECTS]) {
        0 => DEFAULT_SETUP_SECTS,
        n => n,
    };
    (sects + 1) * SECTOR_SIZE
}

pub fn load_bzimage(mem: &GuestMemory, image: &[u8], low_mem_end: u64) -> Result<LoadedKernel> {
    let header = parse_header(image)?;
    let payload = image.get(setup_size(image)..).ok_or(BootError::TooSmall)?;
    if payload.is_empty() {
        return Err(BootError::TooSmall);
    }
    let footprint = header.init_size.max(payload.len() as u64);
    let available = low_mem_end.saturating_sub(KERNEL_START.raw());
    if footprint > available {
        return Err(BootError::KernelTooLarge {
            needed: footprint,
            available,
        });
    }
    mem.write(KERNEL_START, payload)?;
    Ok(LoadedKernel {
        entry: GuestAddress(KERNEL_START.raw() + STARTUP_64_OFFSET),
        end: GuestAddress(KERNEL_START.raw() + footprint),
        header,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn fake_bzimage(payload_len: usize) -> Vec<u8> {
        let setup_sects = 3;
        let mut image = vec![0u8; (setup_sects + 1) * SECTOR_SIZE + payload_len];
        image[OFFSET_SETUP_SECTS] = setup_sects as u8;
        image[OFFSET_BOOT_FLAG..OFFSET_BOOT_FLAG + 2].copy_from_slice(&BOOT_FLAG.to_le_bytes());
        image[0x200] = 0xeb;
        image[OFFSET_JUMP_TARGET] = 0x6a;
        image[OFFSET_HEADER_MAGIC..OFFSET_HEADER_MAGIC + 4].copy_from_slice(HEADER_MAGIC);
        image[OFFSET_VERSION..OFFSET_VERSION + 2].copy_from_slice(&0x020fu16.to_le_bytes());
        image[OFFSET_LOADFLAGS] = LOADED_HIGH;
        image[OFFSET_INITRD_ADDR_MAX..OFFSET_INITRD_ADDR_MAX + 4]
            .copy_from_slice(&0x7fff_ffffu32.to_le_bytes());
        image[OFFSET_XLOADFLAGS..OFFSET_XLOADFLAGS + 2].copy_from_slice(&3u16.to_le_bytes());
        image[OFFSET_CMDLINE_SIZE..OFFSET_CMDLINE_SIZE + 4].copy_from_slice(&2047u32.to_le_bytes());
        image[OFFSET_INIT_SIZE..OFFSET_INIT_SIZE + 4].copy_from_slice(&0x10000u32.to_le_bytes());
        for (i, byte) in image[(setup_sects + 1) * SECTOR_SIZE..]
            .iter_mut()
            .enumerate()
        {
            *byte = i as u8;
        }
        image
    }

    #[test]
    fn parses_valid_header() {
        let header = parse_header(&fake_bzimage(0x1000)).unwrap();
        assert_eq!(header.protocol, 0x020f);
        assert_eq!(header.cmdline_size, 2047);
        assert_eq!(header.initrd_addr_max, 0x7fff_ffff);
        assert_eq!(header.raw().len(), 0x202 + 0x6a - SETUP_HEADER_START);
    }

    #[test]
    fn rejects_bad_images() {
        assert!(matches!(parse_header(&[0; 16]), Err(BootError::TooSmall)));
        let mut image = fake_bzimage(16);
        image[OFFSET_HEADER_MAGIC] = b'X';
        assert!(matches!(parse_header(&image), Err(BootError::BadMagic)));
        let mut image = fake_bzimage(16);
        image[OFFSET_VERSION] = 0x0b;
        assert!(matches!(
            parse_header(&image),
            Err(BootError::UnsupportedProtocol(0x020b))
        ));
        let mut image = fake_bzimage(16);
        image[OFFSET_XLOADFLAGS] = 0;
        assert!(matches!(parse_header(&image), Err(BootError::No64BitEntry)));
        let mut image = fake_bzimage(16);
        image[OFFSET_LOADFLAGS] = 0;
        assert!(matches!(
            parse_header(&image),
            Err(BootError::NotLoadedHigh)
        ));
        let mut image = fake_bzimage(0);
        image.truncate(0x400);
        image[OFFSET_SETUP_SECTS] = 0xff;
        let mem = GuestMemory::new(&[(GuestAddress(0), 0x400000)]).unwrap();
        assert!(load_bzimage(&mem, &image, 0x400000).is_err());
    }

    #[test]
    fn loads_payload_at_one_mib() {
        let mem = GuestMemory::new(&[(GuestAddress(0), 0x400000)]).unwrap();
        let kernel = load_bzimage(&mem, &fake_bzimage(0x2000), 0x400000).unwrap();
        assert_eq!(kernel.entry, GuestAddress(0x100200));
        assert_eq!(kernel.end, GuestAddress(0x110000));
        assert_eq!(mem.read_obj::<u8>(GuestAddress(0x100005)).unwrap(), 5);
    }

    #[test]
    fn rejects_kernel_larger_than_memory() {
        let mem = GuestMemory::new(&[(GuestAddress(0), 0x110000)]).unwrap();
        assert!(matches!(
            load_bzimage(&mem, &fake_bzimage(0x20000), 0x110000),
            Err(BootError::KernelTooLarge { .. })
        ));
    }
}
