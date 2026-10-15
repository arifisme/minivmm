use super::bzimage::{BootError, KernelHeader, LoadedKernel, Result, SETUP_HEADER_START};
use crate::arch::layout::{CMDLINE_MAX_SIZE, CMDLINE_START};
use crate::memory::{GuestAddress, GuestMemory};

const ZERO_PAGE_SIZE: usize = 4096;
const OFFSET_EXT_RAMDISK_IMAGE: usize = 0x0c0;
const OFFSET_EXT_RAMDISK_SIZE: usize = 0x0c4;
const OFFSET_EXT_CMD_LINE_PTR: usize = 0x0c8;
const OFFSET_E820_ENTRIES: usize = 0x1e8;
const OFFSET_VID_MODE: usize = 0x1fa;
const OFFSET_TYPE_OF_LOADER: usize = 0x210;
const OFFSET_LOADFLAGS: usize = 0x211;
const OFFSET_RAMDISK_IMAGE: usize = 0x218;
const OFFSET_RAMDISK_SIZE: usize = 0x21c;
const OFFSET_HEAP_END_PTR: usize = 0x224;
const OFFSET_CMD_LINE_PTR: usize = 0x228;
const OFFSET_E820_TABLE: usize = 0x2d0;
const E820_ENTRY_SIZE: usize = 20;
pub const E820_MAX_ENTRIES: usize = 128;

const LOADER_TYPE_UNDEFINED: u8 = 0xff;
const CAN_USE_HEAP: u8 = 1 << 7;
const HEAP_END: u16 = 0xfe00;
const VID_MODE_NORMAL: u16 = 0xffff;
const PAGE_SIZE: u64 = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum E820Type {
    Ram = 1,
    Reserved = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct E820Entry {
    pub addr: u64,
    pub size: u64,
    pub kind: E820Type,
}

pub struct ZeroPage {
    bytes: Box<[u8; ZERO_PAGE_SIZE]>,
    e820_entries: usize,
}

impl ZeroPage {
    pub fn new(header: &KernelHeader) -> Self {
        let mut bytes = Box::new([0u8; ZERO_PAGE_SIZE]);
        let raw = header.raw();
        bytes[SETUP_HEADER_START..SETUP_HEADER_START + raw.len()].copy_from_slice(raw);
        let mut page = Self {
            bytes,
            e820_entries: 0,
        };
        page.put(OFFSET_TYPE_OF_LOADER, &[LOADER_TYPE_UNDEFINED]);
        let loadflags = page.bytes[OFFSET_LOADFLAGS] | CAN_USE_HEAP;
        page.put(OFFSET_LOADFLAGS, &[loadflags]);
        page.put(OFFSET_HEAP_END_PTR, &HEAP_END.to_le_bytes());
        page.put(OFFSET_VID_MODE, &VID_MODE_NORMAL.to_le_bytes());
        page
    }

    fn put(&mut self, offset: usize, data: &[u8]) {
        self.bytes[offset..offset + data.len()].copy_from_slice(data);
    }

    fn put_split_address(&mut self, low: usize, high: usize, value: u64) {
        self.put(low, &(value as u32).to_le_bytes());
        self.put(high, &((value >> 32) as u32).to_le_bytes());
    }

    pub fn set_cmdline(&mut self, addr: GuestAddress) {
        self.put_split_address(OFFSET_CMD_LINE_PTR, OFFSET_EXT_CMD_LINE_PTR, addr.raw());
    }

    pub fn set_initrd(&mut self, addr: GuestAddress, size: u64) {
        self.put_split_address(OFFSET_RAMDISK_IMAGE, OFFSET_EXT_RAMDISK_IMAGE, addr.raw());
        self.put_split_address(OFFSET_RAMDISK_SIZE, OFFSET_EXT_RAMDISK_SIZE, size);
    }

    pub fn add_e820(&mut self, entry: E820Entry) -> bool {
        if self.e820_entries == E820_MAX_ENTRIES {
            return false;
        }
        let offset = OFFSET_E820_TABLE + self.e820_entries * E820_ENTRY_SIZE;
        self.put(offset, &entry.addr.to_le_bytes());
        self.put(offset + 8, &entry.size.to_le_bytes());
        self.put(offset + 16, &(entry.kind as u32).to_le_bytes());
        self.e820_entries += 1;
        self.put(OFFSET_E820_ENTRIES, &[self.e820_entries as u8]);
        true
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..]
    }

    pub fn write_to(&self, mem: &GuestMemory, addr: GuestAddress) -> Result<()> {
        Ok(mem.write(addr, self.as_bytes())?)
    }
}

pub fn write_cmdline(
    mem: &GuestMemory,
    header: &KernelHeader,
    cmdline: &str,
) -> Result<GuestAddress> {
    if cmdline.as_bytes().contains(&0) {
        return Err(BootError::CmdlineHasNul);
    }
    let max = header.cmdline_size.min(CMDLINE_MAX_SIZE - 1);
    if cmdline.len() > max {
        return Err(BootError::CmdlineTooLong {
            len: cmdline.len(),
            max,
        });
    }
    let mut bytes = cmdline.as_bytes().to_vec();
    bytes.push(0);
    mem.write(CMDLINE_START, &bytes)?;
    Ok(CMDLINE_START)
}

pub fn initrd_address(
    kernel: &LoadedKernel,
    initrd_size: usize,
    low_mem_end: u64,
) -> Result<GuestAddress> {
    let ceiling = low_mem_end.min(kernel.header.initrd_addr_max.saturating_add(1));
    let size = initrd_size as u64;
    let addr = ceiling
        .checked_sub(size)
        .map(|top| top & !(PAGE_SIZE - 1))
        .filter(|addr| *addr >= kernel.end.raw().next_multiple_of(PAGE_SIZE))
        .ok_or(BootError::InitrdTooLarge(initrd_size))?;
    Ok(GuestAddress(addr))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boot::bzimage::tests::{fake_bzimage, set_init_size, set_runtime_address};
    use crate::boot::bzimage::{load_bzimage, parse_header};

    fn u32_at(page: &ZeroPage, offset: usize) -> u32 {
        u32::from_le_bytes(page.as_bytes()[offset..offset + 4].try_into().unwrap())
    }

    #[test]
    fn copies_header_and_sets_loader_fields() {
        let header = parse_header(&fake_bzimage(16)).unwrap();
        let mut page = ZeroPage::new(&header);
        page.set_cmdline(GuestAddress(0x20000));
        page.set_initrd(GuestAddress(0x1_2345_6000), 0x1000);
        let bytes = page.as_bytes();
        assert_eq!(&bytes[0x202..0x206], b"HdrS");
        assert_eq!(bytes[OFFSET_TYPE_OF_LOADER], 0xff);
        assert_ne!(bytes[OFFSET_LOADFLAGS] & CAN_USE_HEAP, 0);
        assert_eq!(u32_at(&page, OFFSET_CMD_LINE_PTR), 0x20000);
        assert_eq!(u32_at(&page, OFFSET_RAMDISK_IMAGE), 0x2345_6000);
        assert_eq!(u32_at(&page, OFFSET_EXT_RAMDISK_IMAGE), 1);
        assert_eq!(u32_at(&page, OFFSET_RAMDISK_SIZE), 0x1000);
    }

    #[test]
    fn e820_table_is_bounded() {
        let header = parse_header(&fake_bzimage(16)).unwrap();
        let mut page = ZeroPage::new(&header);
        let entry = E820Entry {
            addr: 0x100000,
            size: 0x200000,
            kind: E820Type::Ram,
        };
        for _ in 0..E820_MAX_ENTRIES {
            assert!(page.add_e820(entry));
        }
        assert!(!page.add_e820(entry));
        assert_eq!(page.as_bytes()[OFFSET_E820_ENTRIES], 128);
        assert_eq!(u32_at(&page, OFFSET_E820_TABLE + 16), 1);
    }

    #[test]
    fn command_line_limits() {
        let mem = GuestMemory::new(&[(GuestAddress(0), 0x100000)]).unwrap();
        let header = parse_header(&fake_bzimage(16)).unwrap();
        let addr = write_cmdline(&mem, &header, "console=ttyS0").unwrap();
        let mut buf = [0u8; 14];
        mem.read(addr, &mut buf).unwrap();
        assert_eq!(&buf, b"console=ttyS0\0");
        let long = "x".repeat(2048);
        assert!(matches!(
            write_cmdline(&mem, &header, &long),
            Err(BootError::CmdlineTooLong { .. })
        ));
        assert!(write_cmdline(&mem, &header, "a\0b").is_err());
    }

    #[test]
    fn initrd_goes_to_top_of_low_memory() {
        let mem = GuestMemory::new(&[(GuestAddress(0), 0x1000000)]).unwrap();
        let kernel = load_bzimage(&mem, &fake_bzimage(0x2000), 0x1000000).unwrap();
        let addr = initrd_address(&kernel, 0x1800, 0x1000000).unwrap();
        assert_eq!(addr, GuestAddress(0xffe000));
        assert!(initrd_address(&kernel, 0x1000000, 0x1000000).is_err());
        let mut capped = kernel.clone();
        capped.header.initrd_addr_max = 0x7f_ffff;
        assert_eq!(
            initrd_address(&capped, 0x1000, 0x1000000).unwrap(),
            GuestAddress(0x7ff000)
        );
    }

    #[test]
    fn initrd_stays_clear_of_the_kernel_runtime_area() {
        let mem = GuestMemory::new(&[(GuestAddress(0), 0x4000000)]).unwrap();
        let mut image = fake_bzimage(0x2000);
        set_runtime_address(&mut image, 0x1000000, None);
        set_init_size(&mut image, 0x1000000);
        let kernel = load_bzimage(&mem, &image, 0x4000000).unwrap();
        assert!(matches!(
            initrd_address(&kernel, 40 << 20, 0x4000000),
            Err(BootError::InitrdTooLarge(_))
        ));
        assert_eq!(
            initrd_address(&kernel, 32 << 20, 0x4000000).unwrap(),
            GuestAddress(0x2000000)
        );
    }
}
