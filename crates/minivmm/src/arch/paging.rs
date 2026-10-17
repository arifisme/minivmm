use super::layout::{PDE_START, PDPTE_START, PML4_START};
use crate::memory::{GuestAddress, GuestMemory, Result};

const PRESENT: u64 = 1 << 0;
const WRITABLE: u64 = 1 << 1;
const HUGE_PAGE: u64 = 1 << 7;
const ENTRIES_PER_TABLE: u64 = 512;
const HUGE_PAGE_SIZE: u64 = 2 << 20;
const PAGE_SIZE: u64 = 4096;
pub const IDENTITY_MAPPED_GIB: u64 = 4;

pub fn write_identity_page_tables(mem: &GuestMemory) -> Result<GuestAddress> {
    mem.write_obj(PML4_START, &(PDPTE_START.raw() | PRESENT | WRITABLE))?;
    for gib in 0..IDENTITY_MAPPED_GIB {
        let directory = PDE_START.raw() + gib * PAGE_SIZE;
        mem.write_obj(
            GuestAddress(PDPTE_START.raw() + gib * 8),
            &(directory | PRESENT | WRITABLE),
        )?;
        for entry in 0..ENTRIES_PER_TABLE {
            let frame = (gib * ENTRIES_PER_TABLE + entry) * HUGE_PAGE_SIZE;
            mem.write_obj(
                GuestAddress(directory + entry * 8),
                &(frame | PRESENT | WRITABLE | HUGE_PAGE),
            )?;
        }
    }
    Ok(PML4_START)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn translate(mem: &GuestMemory, cr3: GuestAddress, vaddr: u64) -> u64 {
        let pml4e: u64 = mem
            .read_obj(GuestAddress(cr3.raw() + ((vaddr >> 39) & 0x1ff) * 8))
            .unwrap();
        assert_ne!(pml4e & PRESENT, 0);
        let pdpte: u64 = mem
            .read_obj(GuestAddress((pml4e & !0xfff) + ((vaddr >> 30) & 0x1ff) * 8))
            .unwrap();
        assert_ne!(pdpte & PRESENT, 0);
        let pde: u64 = mem
            .read_obj(GuestAddress((pdpte & !0xfff) + ((vaddr >> 21) & 0x1ff) * 8))
            .unwrap();
        assert_ne!(pde & HUGE_PAGE, 0);
        (pde & !0x1f_ffff & !HUGE_PAGE & !0xfff) | (vaddr & 0x1f_ffff)
    }

    #[test]
    fn identity_maps_first_four_gib() {
        let mem = GuestMemory::new(&[(GuestAddress(0), 0x20000)]).unwrap();
        let cr3 = write_identity_page_tables(&mem).unwrap();
        for addr in [0, 0x1234, 0x100200, 0x3fff_ffff, 0xc000_0000, 0xffff_fff0] {
            assert_eq!(translate(&mem, cr3, addr), addr);
        }
    }
}
