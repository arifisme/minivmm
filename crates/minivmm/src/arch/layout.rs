use crate::memory::GuestAddress;

pub const BOOT_GDT_START: GuestAddress = GuestAddress(0x500);
pub const BOOT_IDT_START: GuestAddress = GuestAddress(0x520);
pub const ZERO_PAGE_START: GuestAddress = GuestAddress(0x7000);
pub const BOOT_STACK_POINTER: u64 = 0x8ff0;
pub const PML4_START: GuestAddress = GuestAddress(0x9000);
pub const PDPTE_START: GuestAddress = GuestAddress(0xa000);
pub const PDE_START: GuestAddress = GuestAddress(0xb000);
pub const CMDLINE_START: GuestAddress = GuestAddress(0x20000);
pub const CMDLINE_MAX_SIZE: usize = 0x10000;
pub const EBDA_START: GuestAddress = GuestAddress(0x9fc00);
pub const ACPI_TABLES_START: GuestAddress = GuestAddress(0xe0000);
pub const ACPI_TABLES_END: GuestAddress = GuestAddress(0x100000);
pub const HIGH_MEMORY_START: GuestAddress = GuestAddress(0x100000);
pub const KERNEL_START: GuestAddress = HIGH_MEMORY_START;

pub const MMIO_GAP_START: u64 = 0xc000_0000;
pub const MMIO_GAP_END: u64 = 1 << 32;
pub const PCI_MMIO_START: u64 = MMIO_GAP_START;
pub const PCI_MMIO_END: u64 = IOAPIC_ADDRESS;
pub const IOAPIC_ADDRESS: u64 = 0xfec0_0000;
pub const LAPIC_ADDRESS: u64 = 0xfee0_0000;
pub const KVM_IDENTITY_MAP_START: u64 = 0xfffb_c000;
pub const KVM_TSS_START: u64 = KVM_IDENTITY_MAP_START + 0x1000;

pub const IOAPIC_PINS: u32 = 24;
pub const SERIAL_IRQ: u32 = 4;
pub const SCI_IRQ: u16 = 9;

pub const COM1_PORT: u64 = 0x3f8;
pub const COM_PORT_LEN: u64 = 8;
pub const I8042_PORT: u64 = 0x60;
pub const I8042_PORT_LEN: u64 = 5;
pub const PCI_CONFIG_PORT: u64 = 0xcf8;
pub const PCI_CONFIG_PORT_LEN: u64 = 8;
pub const ACPI_PM_PORT: u16 = 0x600;
pub const ACPI_PM_EVT_LEN: u8 = 4;
pub const ACPI_PM_CNT_LEN: u8 = 2;

pub fn ram_layout(mem_size: u64) -> Vec<(GuestAddress, usize)> {
    if mem_size <= MMIO_GAP_START {
        vec![(GuestAddress(0), mem_size as usize)]
    } else {
        vec![
            (GuestAddress(0), MMIO_GAP_START as usize),
            (
                GuestAddress(MMIO_GAP_END),
                (mem_size - MMIO_GAP_START) as usize,
            ),
        ]
    }
}

pub fn low_memory_end(mem_size: u64) -> u64 {
    mem_size.min(MMIO_GAP_START)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_memory_is_one_region() {
        assert_eq!(ram_layout(512 << 20), vec![(GuestAddress(0), 512 << 20)]);
        assert_eq!(low_memory_end(512 << 20), 512 << 20);
    }

    #[test]
    fn large_memory_skips_the_mmio_gap() {
        let layout = ram_layout(4 << 30);
        assert_eq!(
            layout,
            vec![(GuestAddress(0), 3 << 30), (GuestAddress(4 << 30), 1 << 30)]
        );
        assert_eq!(low_memory_end(4 << 30), 3 << 30);
    }
}
