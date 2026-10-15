use kvm_bindings::{kvm_fpu, kvm_lapic_state, kvm_msr_entry, kvm_regs, kvm_sregs};

use super::gdt::{BOOT_CODE_INDEX, BOOT_DATA_INDEX, BOOT_TSS_INDEX, boot_gdt};
use super::layout::{BOOT_GDT_START, BOOT_IDT_START, BOOT_STACK_POINTER};
use super::paging::write_identity_page_tables;
use crate::memory::{GuestAddress, GuestMemory, Result};

const X86_CR0_PE: u64 = 1 << 0;
const X86_CR0_ET: u64 = 1 << 4;
const X86_CR0_NE: u64 = 1 << 5;
const X86_CR0_PG: u64 = 1 << 31;
const X86_CR4_PAE: u64 = 1 << 5;
const EFER_LME: u64 = 1 << 8;
const EFER_LMA: u64 = 1 << 10;
const RFLAGS_RESERVED: u64 = 1 << 1;

const FPU_CONTROL_WORD: u16 = 0x37f;
const MXCSR_DEFAULT: u32 = 0x1f80;

const MSR_IA32_SYSENTER_CS: u32 = 0x174;
const MSR_IA32_SYSENTER_ESP: u32 = 0x175;
const MSR_IA32_SYSENTER_EIP: u32 = 0x176;
const MSR_IA32_TSC: u32 = 0x10;
const MSR_IA32_MISC_ENABLE: u32 = 0x1a0;
const MSR_MTRR_DEF_TYPE: u32 = 0x2ff;
const MSR_STAR: u32 = 0xc000_0081;
const MSR_LSTAR: u32 = 0xc000_0082;
const MSR_CSTAR: u32 = 0xc000_0083;
const MSR_SYSCALL_MASK: u32 = 0xc000_0084;
const MSR_KERNEL_GS_BASE: u32 = 0xc000_0102;
const MISC_ENABLE_FAST_STRING: u64 = 1;
const MTRR_ENABLE: u64 = 1 << 11;
const MTRR_TYPE_WRITE_BACK: u64 = 6;

const APIC_LVT_LINT0: usize = 0x350;
const APIC_LVT_LINT1: usize = 0x360;
const APIC_MODE_NMI: u32 = 0x4;
const APIC_MODE_EXTINT: u32 = 0x7;

pub fn boot_regs(entry: GuestAddress, zero_page: GuestAddress) -> kvm_regs {
    kvm_regs {
        rip: entry.raw(),
        rsi: zero_page.raw(),
        rsp: BOOT_STACK_POINTER,
        rbp: BOOT_STACK_POINTER,
        rflags: RFLAGS_RESERVED,
        ..Default::default()
    }
}

pub fn configure_long_mode(mem: &GuestMemory, sregs: &mut kvm_sregs) -> Result<()> {
    let gdt = boot_gdt();
    for (index, descriptor) in gdt.iter().enumerate() {
        mem.write_obj(
            GuestAddress(BOOT_GDT_START.raw() + index as u64 * 8),
            &descriptor.0,
        )?;
    }
    mem.write_obj(BOOT_IDT_START, &0u64)?;

    sregs.gdt.base = BOOT_GDT_START.raw();
    sregs.gdt.limit = (size_of_val(&gdt) - 1) as u16;
    sregs.idt.base = BOOT_IDT_START.raw();
    sregs.idt.limit = (size_of::<u64>() - 1) as u16;

    let code = gdt[BOOT_CODE_INDEX as usize].to_kvm_segment(BOOT_CODE_INDEX);
    let data = gdt[BOOT_DATA_INDEX as usize].to_kvm_segment(BOOT_DATA_INDEX);
    sregs.cs = code;
    sregs.ds = data;
    sregs.es = data;
    sregs.fs = data;
    sregs.gs = data;
    sregs.ss = data;
    sregs.tr = gdt[BOOT_TSS_INDEX as usize].to_kvm_segment(BOOT_TSS_INDEX);

    sregs.cr3 = write_identity_page_tables(mem)?.raw();
    sregs.cr4 |= X86_CR4_PAE;
    sregs.cr0 |= X86_CR0_PE | X86_CR0_ET | X86_CR0_NE | X86_CR0_PG;
    sregs.efer |= EFER_LME | EFER_LMA;
    Ok(())
}

pub fn boot_fpu() -> kvm_fpu {
    kvm_fpu {
        fcw: FPU_CONTROL_WORD,
        mxcsr: MXCSR_DEFAULT,
        ..Default::default()
    }
}

pub fn boot_msrs() -> Vec<kvm_msr_entry> {
    let entry = |index, data| kvm_msr_entry {
        index,
        data,
        ..Default::default()
    };
    vec![
        entry(MSR_IA32_SYSENTER_CS, 0),
        entry(MSR_IA32_SYSENTER_ESP, 0),
        entry(MSR_IA32_SYSENTER_EIP, 0),
        entry(MSR_STAR, 0),
        entry(MSR_CSTAR, 0),
        entry(MSR_KERNEL_GS_BASE, 0),
        entry(MSR_SYSCALL_MASK, 0),
        entry(MSR_LSTAR, 0),
        entry(MSR_IA32_TSC, 0),
        entry(MSR_IA32_MISC_ENABLE, MISC_ENABLE_FAST_STRING),
        entry(MSR_MTRR_DEF_TYPE, MTRR_ENABLE | MTRR_TYPE_WRITE_BACK),
    ]
}

fn lapic_register(lapic: &kvm_lapic_state, offset: usize) -> u32 {
    let bytes: [u8; 4] = std::array::from_fn(|i| lapic.regs[offset + i] as u8);
    u32::from_le_bytes(bytes)
}

fn set_lapic_register(lapic: &mut kvm_lapic_state, offset: usize, value: u32) {
    for (i, byte) in value.to_le_bytes().into_iter().enumerate() {
        lapic.regs[offset + i] = byte as _;
    }
}

fn with_delivery_mode(lvt: u32, mode: u32) -> u32 {
    (lvt & !0x700) | (mode << 8)
}

pub fn configure_lapic_lints(lapic: &mut kvm_lapic_state) {
    let lint0 = lapic_register(lapic, APIC_LVT_LINT0);
    set_lapic_register(
        lapic,
        APIC_LVT_LINT0,
        with_delivery_mode(lint0, APIC_MODE_EXTINT),
    );
    let lint1 = lapic_register(lapic, APIC_LVT_LINT1);
    set_lapic_register(
        lapic,
        APIC_LVT_LINT1,
        with_delivery_mode(lint1, APIC_MODE_NMI),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_mode_state() {
        let mem = GuestMemory::new(&[(GuestAddress(0), 0x100000)]).unwrap();
        let mut sregs = kvm_sregs::default();
        configure_long_mode(&mem, &mut sregs).unwrap();
        assert_eq!(
            sregs.cr0 & (X86_CR0_PE | X86_CR0_PG),
            X86_CR0_PE | X86_CR0_PG
        );
        assert_ne!(sregs.cr4 & X86_CR4_PAE, 0);
        assert_eq!(sregs.efer & (EFER_LME | EFER_LMA), EFER_LME | EFER_LMA);
        assert_eq!(sregs.cs.selector, 0x10);
        assert_eq!(sregs.ds.selector, 0x18);
        assert_eq!(sregs.cs.l, 1);
        assert_eq!(sregs.gdt.limit, 39);
        let code: u64 = mem
            .read_obj(GuestAddress(BOOT_GDT_START.raw() + 16))
            .unwrap();
        assert_eq!(code, 0x00af_9b00_0000_ffff);
        assert_eq!(sregs.cr3, 0x9000);
    }

    #[test]
    fn boot_registers() {
        let regs = boot_regs(GuestAddress(0x100200), GuestAddress(0x7000));
        assert_eq!((regs.rip, regs.rsi, regs.rflags), (0x100200, 0x7000, 2));
    }

    #[test]
    fn lint_delivery_modes() {
        let mut lapic = kvm_lapic_state::default();
        set_lapic_register(&mut lapic, APIC_LVT_LINT0, 0x0001_0000);
        configure_lapic_lints(&mut lapic);
        assert_eq!(lapic_register(&lapic, APIC_LVT_LINT0), 0x0001_0700);
        assert_eq!(lapic_register(&lapic, APIC_LVT_LINT1), 0x400);
    }
}
