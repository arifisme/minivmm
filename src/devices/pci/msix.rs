use std::sync::Mutex;

use log::warn;
use serde::{Deserialize, Serialize};

use crate::devices::Interrupt;
use crate::kvm::irq::{GsiRouter, MsiMessage};
use crate::sync::LockExt;

pub const MSIX_CAPABILITY_ID: u8 = 0x11;
pub const MSIX_ENTRY_SIZE: usize = 16;
const MESSAGE_CONTROL_ENABLE: u16 = 1 << 15;
const MESSAGE_CONTROL_FUNCTION_MASK: u16 = 1 << 14;
const VECTOR_CONTROL_MASKED: u32 = 1;
const ADDRESS_LO: usize = 0;
const ADDRESS_HI: usize = 1;
const DATA: usize = 2;
const VECTOR_CONTROL: usize = 3;

pub trait MsiRouter: Send + Sync {
    fn set_route(&self, gsi: u32, message: Option<MsiMessage>);
}

impl MsiRouter for Mutex<GsiRouter> {
    fn set_route(&self, gsi: u32, message: Option<MsiMessage>) {
        if let Err(err) = self.lock_or_recover().set_msi_route(gsi, message) {
            warn!("failed to update MSI route for GSI {gsi}: {err}");
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct MsixEntry {
    fields: [u32; 4],
}

impl Default for MsixEntry {
    fn default() -> Self {
        Self {
            fields: [0, 0, 0, VECTOR_CONTROL_MASKED],
        }
    }
}

impl MsixEntry {
    fn masked(&self) -> bool {
        self.fields[VECTOR_CONTROL] & VECTOR_CONTROL_MASKED != 0
    }

    fn message(&self) -> MsiMessage {
        MsiMessage {
            address: u64::from(self.fields[ADDRESS_LO])
                | (u64::from(self.fields[ADDRESS_HI]) << 32),
            data: self.fields[DATA],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MsixState {
    entries: Vec<MsixEntry>,
    pending: Vec<bool>,
    enabled: bool,
    function_masked: bool,
}

pub struct MsixVector {
    pub gsi: u32,
    pub interrupt: Box<dyn Interrupt>,
}

pub struct Msix {
    state: MsixState,
    vectors: Vec<MsixVector>,
    router: std::sync::Arc<dyn MsiRouter>,
}

impl Msix {
    pub fn new(vectors: Vec<MsixVector>, router: std::sync::Arc<dyn MsiRouter>) -> Self {
        let count = vectors.len();
        Self {
            state: MsixState {
                entries: vec![MsixEntry::default(); count],
                pending: vec![false; count],
                enabled: false,
                function_masked: false,
            },
            vectors,
            router,
        }
    }

    pub fn vector_count(&self) -> usize {
        self.vectors.len()
    }

    pub fn table_size(&self) -> usize {
        self.vectors.len() * MSIX_ENTRY_SIZE
    }

    pub fn pba_size(&self) -> usize {
        self.vectors.len().div_ceil(64) * 8
    }

    pub fn capability_body(
        &self,
        bar: u8,
        table_offset: u32,
        pba_offset: u32,
    ) -> ([u8; 10], [u8; 10]) {
        let control = (self.vectors.len() as u16).saturating_sub(1);
        let mut body = [0u8; 10];
        body[0..2].copy_from_slice(&control.to_le_bytes());
        body[2..6].copy_from_slice(&(table_offset | u32::from(bar)).to_le_bytes());
        body[6..10].copy_from_slice(&(pba_offset | u32::from(bar)).to_le_bytes());
        let mut writable = [0u8; 10];
        writable[1] = ((MESSAGE_CONTROL_ENABLE | MESSAGE_CONTROL_FUNCTION_MASK) >> 8) as u8;
        (body, writable)
    }

    pub fn enabled(&self) -> bool {
        self.state.enabled
    }

    pub fn set_message_control(&mut self, control: u16) {
        self.state.enabled = control & MESSAGE_CONTROL_ENABLE != 0;
        self.state.function_masked = control & MESSAGE_CONTROL_FUNCTION_MASK != 0;
        self.deliver_pending();
    }

    fn deliver_pending(&mut self) {
        if !self.state.enabled || self.state.function_masked {
            return;
        }
        for (index, entry) in self.state.entries.iter().enumerate() {
            if self.state.pending[index] && !entry.masked() {
                self.state.pending[index] = false;
                self.vectors[index].interrupt.trigger();
            }
        }
    }

    fn table_field(&self, offset: u64, len: usize) -> Option<(usize, usize)> {
        if len != 4 || !offset.is_multiple_of(4) {
            return None;
        }
        let offset = usize::try_from(offset).ok()?;
        let entry = offset / MSIX_ENTRY_SIZE;
        (entry < self.state.entries.len()).then_some((entry, (offset % MSIX_ENTRY_SIZE) / 4))
    }

    pub fn read_table(&self, offset: u64, data: &mut [u8]) {
        if data.len() == 8 {
            let (low, high) = data.split_at_mut(4);
            self.read_table(offset, low);
            self.read_table(offset + 4, high);
            return;
        }
        match self.table_field(offset, data.len()) {
            Some((entry, field)) => {
                data.copy_from_slice(&self.state.entries[entry].fields[field].to_le_bytes())
            }
            None => data.fill(0),
        }
    }

    pub fn write_table(&mut self, offset: u64, data: &[u8]) {
        if data.len() == 8 {
            self.write_table(offset, &data[..4]);
            self.write_table(offset + 4, &data[4..]);
            return;
        }
        let Some((entry, field)) = self.table_field(offset, data.len()) else {
            return;
        };
        let value = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        let was_masked = self.state.entries[entry].masked();
        self.state.entries[entry].fields[field] = if field == VECTOR_CONTROL {
            value & VECTOR_CONTROL_MASKED
        } else {
            value
        };
        if field == VECTOR_CONTROL {
            if was_masked && !self.state.entries[entry].masked() {
                self.deliver_pending();
            }
        } else {
            self.update_route(entry);
        }
    }

    fn update_route(&self, entry: usize) {
        let message = self.state.entries[entry].message();
        self.router
            .set_route(self.vectors[entry].gsi, Some(message));
    }

    pub fn read_pba(&self, offset: u64, data: &mut [u8]) {
        for (i, byte) in data.iter_mut().enumerate() {
            let first_vector = (offset as usize).saturating_add(i).saturating_mul(8);
            *byte = (0..8)
                .filter(|bit| {
                    self.state
                        .pending
                        .get(first_vector.saturating_add(*bit))
                        .copied()
                        .unwrap_or(false)
                })
                .fold(0u8, |acc, bit| acc | (1 << bit));
        }
    }

    pub fn notify(&mut self, vector: u16) {
        let index = usize::from(vector);
        if !self.state.enabled || index >= self.vectors.len() {
            return;
        }
        if self.state.function_masked || self.state.entries[index].masked() {
            self.state.pending[index] = true;
        } else {
            self.vectors[index].interrupt.trigger();
        }
    }

    pub fn state(&self) -> MsixState {
        self.state.clone()
    }

    pub fn restore(&mut self, state: MsixState) -> bool {
        let count = self.vectors.len();
        if state.entries.len() != count || state.pending.len() != count {
            return false;
        }
        self.state = state;
        for entry in 0..count {
            self.update_route(entry);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::devices::test_utils::CountingInterrupt;

    #[derive(Default)]
    struct RecordingRouter(Mutex<Vec<(u32, Option<MsiMessage>)>>);

    impl MsiRouter for RecordingRouter {
        fn set_route(&self, gsi: u32, message: Option<MsiMessage>) {
            self.0.lock().unwrap().push((gsi, message));
        }
    }

    fn msix(count: usize) -> (Msix, Vec<CountingInterrupt>, Arc<RecordingRouter>) {
        let interrupts: Vec<CountingInterrupt> =
            (0..count).map(|_| CountingInterrupt::default()).collect();
        let vectors = interrupts
            .iter()
            .enumerate()
            .map(|(i, irq)| MsixVector {
                gsi: 24 + i as u32,
                interrupt: Box::new(irq.clone()),
            })
            .collect();
        let router = Arc::new(RecordingRouter::default());
        (Msix::new(vectors, router.clone()), interrupts, router)
    }

    fn write_u32(msix: &mut Msix, offset: u64, value: u32) {
        msix.write_table(offset, &value.to_le_bytes());
    }

    #[test]
    fn programs_routes_from_table_writes() {
        let (mut msix, _, router) = msix(2);
        write_u32(&mut msix, 16, 0xfee0_0000);
        write_u32(&mut msix, 20, 0);
        write_u32(&mut msix, 24, 0x41);
        let routes = router.0.lock().unwrap();
        assert_eq!(
            routes.last(),
            Some(&(
                25,
                Some(MsiMessage {
                    address: 0xfee0_0000,
                    data: 0x41
                })
            ))
        );
        let mut data = [0u8; 4];
        msix.read_table(24, &mut data);
        assert_eq!(u32::from_le_bytes(data), 0x41);
    }

    #[test]
    fn masked_vectors_become_pending() {
        let (mut msix, irqs, _) = msix(2);
        msix.notify(0);
        assert_eq!(irqs[0].count(), 0, "disabled MSI-X must not fire");
        msix.set_message_control(MESSAGE_CONTROL_ENABLE);
        msix.notify(0);
        assert_eq!(irqs[0].count(), 0);
        let mut pba = [0u8; 8];
        msix.read_pba(0, &mut pba);
        assert_eq!(pba[0], 1);
        write_u32(&mut msix, 12, 0);
        assert_eq!(irqs[0].count(), 1);
        msix.read_pba(0, &mut pba);
        assert_eq!(pba[0], 0);
        msix.notify(0);
        assert_eq!(irqs[0].count(), 2);
    }

    #[test]
    fn function_mask_defers_delivery() {
        let (mut msix, irqs, _) = msix(1);
        write_u32(&mut msix, 12, 0);
        msix.set_message_control(MESSAGE_CONTROL_ENABLE | MESSAGE_CONTROL_FUNCTION_MASK);
        msix.notify(0);
        assert_eq!(irqs[0].count(), 0);
        msix.set_message_control(MESSAGE_CONTROL_ENABLE);
        assert_eq!(irqs[0].count(), 1);
    }

    #[test]
    fn rejects_bad_accesses() {
        let (mut msix, irqs, router) = msix(1);
        msix.write_table(16, &[0; 4]);
        msix.write_table(2, &[0; 4]);
        msix.write_table(0, &[0; 2]);
        msix.notify(7);
        assert!(router.0.lock().unwrap().is_empty());
        assert_eq!(irqs[0].count(), 0);
        let mut data = [0xffu8; 4];
        msix.read_table(64, &mut data);
        assert_eq!(data, [0; 4]);
        let mut pba = [0u8; 4];
        msix.read_pba(u64::MAX, &mut pba);
    }

    #[test]
    fn capability_layout() {
        let (msix, _, _) = msix(3);
        let (body, writable) = msix.capability_body(0, 0x4000, 0x5000);
        assert_eq!(u16::from_le_bytes([body[0], body[1]]), 2);
        assert_eq!(u32::from_le_bytes(body[2..6].try_into().unwrap()), 0x4000);
        assert_eq!(writable[1], 0xc0);
        assert_eq!(msix.table_size(), 48);
        assert_eq!(msix.pba_size(), 8);
    }

    #[test]
    fn state_round_trip() {
        let (mut msix, _, _) = msix(2);
        write_u32(&mut msix, 0, 0xfee0_1000);
        msix.set_message_control(MESSAGE_CONTROL_ENABLE);
        let state = msix.state();
        let (mut other, _, router) = self::msix(2);
        assert!(other.restore(state.clone()));
        assert_eq!(other.state(), state);
        assert_eq!(router.0.lock().unwrap().len(), 2);
        let (mut wrong, _, _) = self::msix(3);
        assert!(!wrong.restore(state));
    }
}
