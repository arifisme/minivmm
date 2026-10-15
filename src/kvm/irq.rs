use std::collections::BTreeMap;
use std::sync::Arc;

use kvm_bindings::{
    KVM_IRQ_ROUTING_IRQCHIP, KVM_IRQ_ROUTING_MSI, KVM_IRQCHIP_IOAPIC, KVM_IRQCHIP_PIC_MASTER,
    KVM_IRQCHIP_PIC_SLAVE, KvmIrqRouting, kvm_irq_routing_entry,
};
use kvm_ioctls::VmFd;
use serde::{Deserialize, Serialize};

use super::{IoctlContext, KvmError, Result};
use crate::arch::layout::IOAPIC_PINS;

const PIC_IRQS: u32 = 16;
const PIC_PINS_PER_CHIP: u32 = 8;
const MAX_GSI: u32 = 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MsiMessage {
    pub address: u64,
    pub data: u32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RoutingState {
    next_gsi: u32,
    msi_routes: BTreeMap<u32, MsiMessage>,
}

pub struct GsiRouter {
    vm: Arc<VmFd>,
    state: RoutingState,
}

fn irqchip_entry(gsi: u32, irqchip: u32, pin: u32) -> kvm_irq_routing_entry {
    let mut entry = kvm_irq_routing_entry {
        gsi,
        type_: KVM_IRQ_ROUTING_IRQCHIP,
        ..Default::default()
    };
    entry.u.irqchip.irqchip = irqchip;
    entry.u.irqchip.pin = pin;
    entry
}

fn msi_entry(gsi: u32, message: MsiMessage) -> kvm_irq_routing_entry {
    let mut entry = kvm_irq_routing_entry {
        gsi,
        type_: KVM_IRQ_ROUTING_MSI,
        ..Default::default()
    };
    entry.u.msi.address_lo = message.address as u32;
    entry.u.msi.address_hi = (message.address >> 32) as u32;
    entry.u.msi.data = message.data;
    entry
}

pub fn routing_entries(state: &RoutingState) -> Vec<kvm_irq_routing_entry> {
    let mut entries = Vec::new();
    for gsi in 0..PIC_IRQS {
        let chip = if gsi < PIC_PINS_PER_CHIP {
            KVM_IRQCHIP_PIC_MASTER
        } else {
            KVM_IRQCHIP_PIC_SLAVE
        };
        entries.push(irqchip_entry(gsi, chip, gsi % PIC_PINS_PER_CHIP));
    }
    for pin in 0..IOAPIC_PINS {
        entries.push(irqchip_entry(pin, KVM_IRQCHIP_IOAPIC, pin));
    }
    for (gsi, message) in &state.msi_routes {
        entries.push(msi_entry(*gsi, *message));
    }
    entries
}

impl GsiRouter {
    pub fn new(vm: Arc<VmFd>) -> Self {
        Self {
            vm,
            state: RoutingState {
                next_gsi: IOAPIC_PINS,
                msi_routes: BTreeMap::new(),
            },
        }
    }

    pub fn allocate(&mut self) -> Result<u32> {
        if self.state.next_gsi >= MAX_GSI {
            return Err(KvmError::GsiExhausted);
        }
        let gsi = self.state.next_gsi;
        self.state.next_gsi += 1;
        Ok(gsi)
    }

    pub fn set_msi_route(&mut self, gsi: u32, message: Option<MsiMessage>) -> Result<()> {
        let changed = match message {
            Some(message) => self.state.msi_routes.insert(gsi, message) != Some(message),
            None => self.state.msi_routes.remove(&gsi).is_some(),
        };
        if changed { self.commit() } else { Ok(()) }
    }

    pub fn commit(&self) -> Result<()> {
        let entries = routing_entries(&self.state);
        let routing = KvmIrqRouting::from_entries(&entries).map_err(|_| KvmError::GsiExhausted)?;
        self.vm.set_gsi_routing(&routing).ctx("KVM_SET_GSI_ROUTING")
    }

    pub fn state(&self) -> RoutingState {
        self.state.clone()
    }

    pub fn restore(&mut self, state: RoutingState) -> Result<()> {
        self.state = state;
        self.commit()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_routes_cover_pic_and_ioapic() {
        let entries = routing_entries(&RoutingState::default());
        assert_eq!(entries.len(), 16 + 24);
        // SAFETY: entries were built as irqchip routes.
        let pins: Vec<_> = entries
            .iter()
            .map(|e| unsafe { (e.gsi, e.u.irqchip.irqchip, e.u.irqchip.pin) })
            .collect();
        assert!(pins.contains(&(9, KVM_IRQCHIP_PIC_SLAVE, 1)));
        assert!(pins.contains(&(4, KVM_IRQCHIP_PIC_MASTER, 4)));
        assert!(pins.contains(&(23, KVM_IRQCHIP_IOAPIC, 23)));
    }

    #[test]
    fn msi_routes_are_appended() {
        let mut state = RoutingState::default();
        state.msi_routes.insert(
            30,
            MsiMessage {
                address: 0x1_fee0_1000,
                data: 0x4041,
            },
        );
        let entries = routing_entries(&state);
        let msi = entries.last().unwrap();
        assert_eq!((msi.gsi, msi.type_), (30, KVM_IRQ_ROUTING_MSI));
        // SAFETY: the entry was built as an MSI route.
        let (lo, hi, data) =
            unsafe { (msi.u.msi.address_lo, msi.u.msi.address_hi, msi.u.msi.data) };
        assert_eq!((lo, hi, data), (0xfee0_1000, 1, 0x4041));
    }
}
