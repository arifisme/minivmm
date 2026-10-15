use std::sync::{Arc, Mutex};

use log::warn;
use serde::{Deserialize, Serialize};

use super::PciDevice;
use super::config::{BarChange, PciConfigState, PciConfiguration, PciIds};
use crate::devices::bus::{Bus, BusDevice};
use crate::sync::LockExt;

pub const MAX_DEVICES: usize = 32;
const ADDRESS_ENABLE: u32 = 1 << 31;
const ADDRESS_REGISTER_MASK: u32 = 0x80ff_fffc;
const DATA_OFFSET: u64 = 4;

const HOST_BRIDGE_IDS: PciIds = PciIds {
    vendor: 0x1b36,
    device: 0x0008,
    class: 0x06,
    subclass: 0x00,
    prog_if: 0,
    revision: 0,
    subsystem_vendor: 0x1b36,
    subsystem: 0x0008,
};

pub struct PciHostBridge {
    config: PciConfiguration,
}

impl PciHostBridge {
    pub fn new() -> Self {
        Self {
            config: PciConfiguration::new(HOST_BRIDGE_IDS),
        }
    }
}

impl Default for PciHostBridge {
    fn default() -> Self {
        Self::new()
    }
}

impl PciDevice for PciHostBridge {
    fn read_config(&mut self, offset: usize, data: &mut [u8]) {
        self.config.read(offset, data);
    }

    fn write_config(&mut self, offset: usize, data: &[u8]) -> Option<BarChange> {
        self.config.write(offset, data)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PciRootState {
    address: u32,
    host_bridge: PciConfigState,
}

pub struct PciConfigIo {
    address: u32,
    host_bridge: Arc<Mutex<PciHostBridge>>,
    devices: Vec<Option<Arc<Mutex<dyn PciDevice>>>>,
    mmio: Arc<Bus>,
}

struct ConfigAddress {
    device: usize,
    register: usize,
}

impl PciConfigIo {
    pub fn new(mmio: Arc<Bus>) -> Self {
        let host_bridge = Arc::new(Mutex::new(PciHostBridge::new()));
        let mut devices: Vec<Option<Arc<Mutex<dyn PciDevice>>>> = vec![None; MAX_DEVICES];
        devices[0] = Some(host_bridge.clone());
        Self {
            address: 0,
            host_bridge,
            devices,
            mmio,
        }
    }

    pub fn state(&self) -> PciRootState {
        PciRootState {
            address: self.address,
            host_bridge: self.host_bridge.lock_or_recover().config.state(),
        }
    }

    pub fn restore(&mut self, state: &PciRootState) -> bool {
        self.address = state.address & ADDRESS_REGISTER_MASK;
        self.host_bridge
            .lock_or_recover()
            .config
            .restore(&state.host_bridge)
    }

    pub fn add_device(&mut self, device: Arc<Mutex<dyn PciDevice>>) -> Option<usize> {
        let slot = self.devices.iter().position(Option::is_none)?;
        self.devices[slot] = Some(device);
        Some(slot)
    }

    fn decode(&self) -> Option<ConfigAddress> {
        let enabled = self.address & ADDRESS_ENABLE != 0;
        let bus = (self.address >> 16) & 0xff;
        let device = ((self.address >> 11) & 0x1f) as usize;
        let function = (self.address >> 8) & 0x7;
        let register = (self.address & 0xfc) as usize;
        (enabled && bus == 0 && function == 0).then_some(ConfigAddress { device, register })
    }

    fn target(&self, offset: u64, len: usize) -> Option<(Arc<Mutex<dyn PciDevice>>, usize)> {
        let byte = usize::try_from(offset.checked_sub(DATA_OFFSET)?).ok()?;
        if byte + len > 4 {
            return None;
        }
        let address = self.decode()?;
        let device = self.devices.get(address.device)?.clone()?;
        Some((device, address.register + byte))
    }

    fn apply_bar_change(&self, change: BarChange) {
        if let Err(err) = self.mmio.relocate(change.old, change.new) {
            warn!(
                "cannot move BAR{} from {:#x} to {:#x}: {err}",
                change.index, change.old, change.new
            );
        }
    }
}

impl BusDevice for PciConfigIo {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        if offset == 0 && data.len() == 4 {
            data.copy_from_slice(&self.address.to_le_bytes());
            return;
        }
        match self.target(offset, data.len()) {
            Some((device, register)) => device.lock_or_recover().read_config(register, data),
            None => data.fill(0xff),
        }
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        if offset == 0 && data.len() == 4 {
            let value = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
            self.address = value & ADDRESS_REGISTER_MASK;
            return;
        }
        if let Some((device, register)) = self.target(offset, data.len()) {
            let change = device.lock_or_recover().write_config(register, data);
            if let Some(change) = change {
                self.apply_bar_change(change);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Probe {
        config: PciConfiguration,
    }

    impl PciDevice for Probe {
        fn read_config(&mut self, offset: usize, data: &mut [u8]) {
            self.config.read(offset, data);
        }

        fn write_config(&mut self, offset: usize, data: &[u8]) -> Option<BarChange> {
            self.config.write(offset, data)
        }
    }

    struct Sink;

    impl BusDevice for Sink {
        fn read(&mut self, _: u64, data: &mut [u8]) {
            data.fill(0xaa);
        }

        fn write(&mut self, _: u64, _: &[u8]) {}
    }

    fn select(io: &mut PciConfigIo, device: u32, register: u32) {
        io.write(
            0,
            &(ADDRESS_ENABLE | (device << 11) | register).to_le_bytes(),
        );
    }

    fn read_u32(io: &mut PciConfigIo) -> u32 {
        let mut data = [0u8; 4];
        io.read(4, &mut data);
        u32::from_le_bytes(data)
    }

    fn setup() -> (PciConfigIo, Arc<Bus>) {
        let mmio = Arc::new(Bus::new());
        let mut io = PciConfigIo::new(mmio.clone());
        let mut config = PciConfiguration::new(PciIds {
            vendor: 0x1af4,
            device: 0x1041,
            class: 2,
            subclass: 0,
            prog_if: 0,
            revision: 1,
            subsystem_vendor: 0x1af4,
            subsystem: 0x40,
        });
        config.add_memory_bar(0, 0xc000_0000, 0x1000);
        assert_eq!(
            io.add_device(Arc::new(Mutex::new(Probe { config }))),
            Some(1)
        );
        mmio.insert(Arc::new(Mutex::new(Sink)), 0xc000_0000, 0x1000)
            .unwrap();
        (io, mmio)
    }

    #[test]
    fn enumerates_devices() {
        let (mut io, _) = setup();
        select(&mut io, 0, 0);
        assert_eq!(read_u32(&mut io), 0x0008_1b36);
        select(&mut io, 1, 0);
        assert_eq!(read_u32(&mut io), 0x1041_1af4);
        select(&mut io, 2, 0);
        assert_eq!(read_u32(&mut io), 0xffff_ffff);
        io.write(0, &((1 << 16) | (1 << 11) | ADDRESS_ENABLE).to_le_bytes());
        assert_eq!(read_u32(&mut io), 0xffff_ffff);
        io.write(0, &(1u32 << 11).to_le_bytes());
        assert_eq!(read_u32(&mut io), 0xffff_ffff);
    }

    #[test]
    fn sub_dword_accesses() {
        let (mut io, _) = setup();
        select(&mut io, 1, 0);
        let mut word = [0u8; 2];
        io.read(6, &mut word);
        assert_eq!(u16::from_le_bytes(word), 0x1041);
        let mut byte = [0u8; 1];
        io.read(7, &mut byte);
        assert_eq!(byte[0], 0x10);
        let mut straddle = [0u8; 4];
        io.read(6, &mut straddle);
        assert_eq!(straddle, [0xff; 4]);
        let mut address = [0u8; 4];
        io.read(0, &mut address);
        assert_eq!(u32::from_le_bytes(address), ADDRESS_ENABLE | (1 << 11));
    }

    #[test]
    fn state_round_trip() {
        let (mut io, mmio) = setup();
        select(&mut io, 0, 4);
        io.write(4, &[0x06, 0, 0, 0]);
        let state = io.state();
        let mut other = PciConfigIo::new(mmio);
        assert!(other.restore(&state));
        assert_eq!(read_u32(&mut other) & 0xffff, 0x6);
    }

    #[test]
    fn bar_moves_update_the_mmio_bus() {
        let (mut io, mmio) = setup();
        select(&mut io, 1, 0x10);
        io.write(4, &[0xff; 4]);
        assert_eq!(read_u32(&mut io), 0xffff_f000);
        io.write(4, &0xd000_0000u32.to_le_bytes());
        let mut data = [0u8; 1];
        assert!(mmio.read(0xd000_0000, &mut data));
        assert!(!mmio.read(0xc000_0000, &mut data));
    }
}
