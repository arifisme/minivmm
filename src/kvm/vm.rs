use std::sync::Arc;

use kvm_bindings::{
    KVM_IRQCHIP_IOAPIC, KVM_IRQCHIP_PIC_MASTER, KVM_IRQCHIP_PIC_SLAVE, KVM_PIT_SPEAKER_DUMMY,
    kvm_clock_data, kvm_irqchip, kvm_pit_config, kvm_pit_state2, kvm_userspace_memory_region,
};
use kvm_ioctls::{Kvm, VmFd};
use serde::{Deserialize, Serialize};

use super::{IoctlContext, Result};
use crate::arch::layout::{KVM_IDENTITY_MAP_START, KVM_TSS_START};
use crate::memory::GuestMemory;

#[derive(Clone, Serialize, Deserialize)]
pub struct VmState {
    pic_master: kvm_irqchip,
    pic_slave: kvm_irqchip,
    ioapic: kvm_irqchip,
    pit: kvm_pit_state2,
    clock: kvm_clock_data,
}

pub fn create_vm(kvm: &Kvm, memory: &GuestMemory) -> Result<Arc<VmFd>> {
    let vm = kvm.create_vm().ctx("KVM_CREATE_VM")?;
    for (slot, region) in memory.regions().iter().enumerate() {
        let descriptor = kvm_userspace_memory_region {
            slot: slot as u32,
            flags: 0,
            guest_phys_addr: region.start().raw(),
            memory_size: region.len() as u64,
            userspace_addr: region.host_address(),
        };
        // SAFETY: the region stays mapped for as long as the VM exists because
        // the guest memory is kept alive alongside the VM file descriptor.
        unsafe { vm.set_user_memory_region(descriptor) }.ctx("KVM_SET_USER_MEMORY_REGION")?;
    }
    vm.set_identity_map_address(KVM_IDENTITY_MAP_START)
        .ctx("KVM_SET_IDENTITY_MAP_ADDR")?;
    vm.set_tss_address(KVM_TSS_START as usize)
        .ctx("KVM_SET_TSS_ADDR")?;
    vm.create_irq_chip().ctx("KVM_CREATE_IRQCHIP")?;
    let pit = kvm_pit_config {
        flags: KVM_PIT_SPEAKER_DUMMY,
        ..Default::default()
    };
    vm.create_pit2(pit).ctx("KVM_CREATE_PIT2")?;
    Ok(Arc::new(vm))
}

fn get_irqchip(vm: &VmFd, chip_id: u32) -> Result<kvm_irqchip> {
    let mut chip = kvm_irqchip {
        chip_id,
        ..Default::default()
    };
    vm.get_irqchip(&mut chip).ctx("KVM_GET_IRQCHIP")?;
    Ok(chip)
}

const KVM_CLOCK_SETTABLE_FLAGS: u32 = 0;

pub fn save_vm_state(vm: &VmFd) -> Result<VmState> {
    let mut clock = vm.get_clock().ctx("KVM_GET_CLOCK")?;
    clock.flags = KVM_CLOCK_SETTABLE_FLAGS;
    Ok(VmState {
        pic_master: get_irqchip(vm, KVM_IRQCHIP_PIC_MASTER)?,
        pic_slave: get_irqchip(vm, KVM_IRQCHIP_PIC_SLAVE)?,
        ioapic: get_irqchip(vm, KVM_IRQCHIP_IOAPIC)?,
        pit: vm.get_pit2().ctx("KVM_GET_PIT2")?,
        clock,
    })
}

pub fn restore_vm_state(vm: &VmFd, state: &VmState) -> Result<()> {
    vm.set_pit2(&state.pit).ctx("KVM_SET_PIT2")?;
    vm.set_clock(&state.clock).ctx("KVM_SET_CLOCK")?;
    for chip in [&state.pic_master, &state.pic_slave, &state.ioapic] {
        vm.set_irqchip(chip).ctx("KVM_SET_IRQCHIP")?;
    }
    Ok(())
}
