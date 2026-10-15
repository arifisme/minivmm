use serde::{Deserialize, Serialize};

pub const CONFIG_SPACE_SIZE: usize = 256;
const REGISTER_COUNT: usize = CONFIG_SPACE_SIZE / 4;
const VENDOR_DEVICE_REG: usize = 0;
const COMMAND_STATUS_REG: usize = 1;
const CLASS_REG: usize = 2;
const HEADER_TYPE_REG: usize = 3;
const BAR0_REG: usize = 4;
pub const BAR_COUNT: usize = 6;
const SUBSYSTEM_REG: usize = 11;
const CAPABILITY_POINTER_REG: usize = 13;
const INTERRUPT_REG: usize = 15;
const FIRST_CAPABILITY_OFFSET: usize = 0x40;

const COMMAND_WRITABLE: u32 = 0x0407;
const STATUS_CAPABILITIES_LIST: u32 = 1 << 4;
const INTERRUPT_LINE_WRITABLE: u32 = 0xff;
const BAR_MEMORY_ADDRESS_MASK: u32 = !0xf;

#[derive(Clone, Copy, Debug)]
pub struct PciIds {
    pub vendor: u16,
    pub device: u16,
    pub class: u8,
    pub subclass: u8,
    pub prog_if: u8,
    pub revision: u8,
    pub subsystem_vendor: u16,
    pub subsystem: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BarChange {
    pub index: usize,
    pub old: u64,
    pub new: u64,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
struct Bar {
    size: u32,
    address: u32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PciConfigState {
    registers: Vec<u32>,
    bars: Vec<(u32, u32)>,
}

#[derive(Clone, Debug)]
pub struct PciConfiguration {
    registers: [u32; REGISTER_COUNT],
    writable: [u32; REGISTER_COUNT],
    bars: [Bar; BAR_COUNT],
    last_capability: Option<usize>,
    next_capability: usize,
}

impl PciConfiguration {
    pub fn new(ids: PciIds) -> Self {
        let mut config = Self {
            registers: [0; REGISTER_COUNT],
            writable: [0; REGISTER_COUNT],
            bars: [Bar::default(); BAR_COUNT],
            last_capability: None,
            next_capability: FIRST_CAPABILITY_OFFSET,
        };
        config.registers[VENDOR_DEVICE_REG] = u32::from(ids.vendor) | (u32::from(ids.device) << 16);
        config.registers[CLASS_REG] = (u32::from(ids.class) << 24)
            | (u32::from(ids.subclass) << 16)
            | (u32::from(ids.prog_if) << 8)
            | u32::from(ids.revision);
        config.registers[SUBSYSTEM_REG] =
            u32::from(ids.subsystem_vendor) | (u32::from(ids.subsystem) << 16);
        config.writable[COMMAND_STATUS_REG] = COMMAND_WRITABLE;
        config.writable[INTERRUPT_REG] = INTERRUPT_LINE_WRITABLE;
        config.registers[HEADER_TYPE_REG] = 0;
        config
    }

    pub fn add_memory_bar(&mut self, index: usize, address: u32, size: u32) {
        debug_assert!(index < BAR_COUNT && size.is_power_of_two() && size >= 16);
        debug_assert_eq!(address % size, 0);
        self.bars[index] = Bar { size, address };
        self.registers[BAR0_REG + index] = address;
        self.writable[BAR0_REG + index] = !(size - 1) & BAR_MEMORY_ADDRESS_MASK;
    }

    pub fn bar_address(&self, index: usize) -> u64 {
        u64::from(self.bars[index].address)
    }

    pub fn add_capability(&mut self, id: u8, body: &[u8], writable: &[u8]) -> usize {
        let offset = self.next_capability;
        let len = 2 + body.len();
        debug_assert!(offset + len <= CONFIG_SPACE_SIZE);
        let mut bytes = vec![id, 0];
        bytes.extend_from_slice(body);
        let mut mask = vec![0u8; 2];
        mask.extend(
            body.iter()
                .enumerate()
                .map(|(i, _)| writable.get(i).copied().unwrap_or(0)),
        );
        for (i, (byte, mask)) in bytes.iter().zip(&mask).enumerate() {
            self.set_byte(offset + i, *byte);
            self.set_writable_byte(offset + i, *mask);
        }
        match self.last_capability {
            Some(previous) => self.set_byte(previous + 1, offset as u8),
            None => {
                self.registers[CAPABILITY_POINTER_REG] = offset as u32;
                self.registers[COMMAND_STATUS_REG] |= STATUS_CAPABILITIES_LIST << 16;
            }
        }
        self.last_capability = Some(offset);
        self.next_capability = (offset + len).next_multiple_of(4);
        offset
    }

    fn set_byte(&mut self, offset: usize, value: u8) {
        let shift = (offset % 4) * 8;
        let reg = &mut self.registers[offset / 4];
        *reg = (*reg & !(0xff << shift)) | (u32::from(value) << shift);
    }

    fn set_writable_byte(&mut self, offset: usize, value: u8) {
        let shift = (offset % 4) * 8;
        let reg = &mut self.writable[offset / 4];
        *reg = (*reg & !(0xff << shift)) | (u32::from(value) << shift);
    }

    pub fn byte(&self, offset: usize) -> u8 {
        self.registers
            .get(offset / 4)
            .map_or(0, |reg| (reg >> ((offset % 4) * 8)) as u8)
    }

    pub fn u16_at(&self, offset: usize) -> u16 {
        u16::from_le_bytes([self.byte(offset), self.byte(offset + 1)])
    }

    pub fn u32_at(&self, offset: usize) -> u32 {
        u32::from_le_bytes(std::array::from_fn(|i| self.byte(offset + i)))
    }

    pub fn read(&self, offset: usize, data: &mut [u8]) {
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = self.byte(offset + i);
        }
    }

    pub fn write(&mut self, offset: usize, data: &[u8]) -> Option<BarChange> {
        if offset >= CONFIG_SPACE_SIZE || data.len() > CONFIG_SPACE_SIZE - offset {
            return None;
        }
        for (i, value) in data.iter().enumerate() {
            let at = offset + i;
            let shift = (at % 4) * 8;
            let mask = ((self.writable[at / 4] >> shift) & 0xff) as u8;
            let old = self.byte(at);
            self.set_byte(at, (old & !mask) | (value & mask));
        }
        let first_reg = offset / 4;
        let last_reg = (offset + data.len() - 1) / 4;
        (first_reg..=last_reg)
            .filter_map(|reg| reg.checked_sub(BAR0_REG))
            .filter(|index| *index < BAR_COUNT)
            .find_map(|index| self.commit_bar(index))
    }

    fn commit_bar(&mut self, index: usize) -> Option<BarChange> {
        let bar = self.bars[index];
        if bar.size == 0 {
            return None;
        }
        let address = self.registers[BAR0_REG + index] & BAR_MEMORY_ADDRESS_MASK;
        let sizing_pattern = self.writable[BAR0_REG + index];
        if address == sizing_pattern || address == bar.address {
            return None;
        }
        self.bars[index].address = address;
        Some(BarChange {
            index,
            old: u64::from(bar.address),
            new: u64::from(address),
        })
    }

    pub fn state(&self) -> PciConfigState {
        PciConfigState {
            registers: self.registers.to_vec(),
            bars: self.bars.iter().map(|b| (b.size, b.address)).collect(),
        }
    }

    pub fn restore(&mut self, state: &PciConfigState) -> bool {
        if state.registers.len() != REGISTER_COUNT || state.bars.len() != BAR_COUNT {
            return false;
        }
        let sizes_match = state
            .bars
            .iter()
            .zip(&self.bars)
            .all(|((size, _), bar)| *size == bar.size);
        if !sizes_match {
            return false;
        }
        self.registers.copy_from_slice(&state.registers);
        for (bar, (_, address)) in self.bars.iter_mut().zip(&state.bars) {
            bar.address = *address;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> PciIds {
        PciIds {
            vendor: 0x1af4,
            device: 0x1042,
            class: 0x01,
            subclass: 0x80,
            prog_if: 0,
            revision: 1,
            subsystem_vendor: 0x1af4,
            subsystem: 0x40,
        }
    }

    #[test]
    fn identity_registers() {
        let config = PciConfiguration::new(ids());
        assert_eq!(config.u32_at(0), 0x1042_1af4);
        assert_eq!(config.u32_at(8), 0x0180_0001);
        assert_eq!(config.u32_at(0x2c), 0x0040_1af4);
    }

    #[test]
    fn read_only_registers_ignore_writes() {
        let mut config = PciConfiguration::new(ids());
        config.write(0, &[0xff; 4]);
        config.write(8, &[0xff; 4]);
        assert_eq!(config.u32_at(0), 0x1042_1af4);
        config.write(4, &[0xff, 0xff, 0xff, 0xff]);
        assert_eq!(config.u16_at(4), 0x0407);
        assert_eq!(config.u16_at(6), 0);
    }

    #[test]
    fn bar_sizing_and_relocation() {
        let mut config = PciConfiguration::new(ids());
        config.add_memory_bar(0, 0xc000_0000, 0x8000);
        assert_eq!(config.write(0x10, &[0xff; 4]), None);
        assert_eq!(config.u32_at(0x10), 0xffff_8000);
        assert_eq!(config.write(0x10, &0xc000_0000u32.to_le_bytes()), None);
        assert_eq!(config.u32_at(0x10), 0xc000_0000);
        let change = config.write(0x10, &0xd000_0000u32.to_le_bytes());
        assert_eq!(
            change,
            Some(BarChange {
                index: 0,
                old: 0xc000_0000,
                new: 0xd000_0000
            })
        );
        assert_eq!(config.bar_address(0), 0xd000_0000);
        assert_eq!(config.write(0x14, &[0xff; 4]), None);
        assert_eq!(config.u32_at(0x14), 0);
    }

    #[test]
    fn capability_chain() {
        let mut config = PciConfiguration::new(ids());
        let first = config.add_capability(0x09, &[16, 1, 0, 0], &[0, 0, 0xff, 0]);
        let second = config.add_capability(0x11, &[0x01, 0x00, 0, 0, 0, 0, 0, 0, 0, 0], &[]);
        assert_eq!(first, 0x40);
        assert_eq!(second, 0x48);
        assert_eq!(config.byte(0x34), 0x40);
        assert_ne!(config.u16_at(6) & 0x10, 0);
        assert_eq!(config.byte(0x41), 0x48);
        assert_eq!(config.byte(0x49), 0);
        config.write(0x44, &[1, 2, 3, 4]);
        assert_eq!(config.u32_at(0x44), 1);
        assert_eq!(config.u32_at(0x40), 0x0110_4809);
    }

    #[test]
    fn out_of_range_accesses() {
        let mut config = PciConfiguration::new(ids());
        assert_eq!(config.write(0xfe, &[1, 2, 3, 4]), None);
        assert_eq!(config.write(0x1000, &[1]), None);
        let mut data = [0u8; 4];
        config.read(0xfe, &mut data);
        assert_eq!(&data[2..], &[0, 0]);
    }

    #[test]
    fn state_round_trip() {
        let mut config = PciConfiguration::new(ids());
        config.add_memory_bar(0, 0xc000_0000, 0x8000);
        config.write(4, &[0x06, 0]);
        let state = config.state();
        let mut other = PciConfiguration::new(ids());
        other.add_memory_bar(0, 0xc000_0000, 0x8000);
        assert!(other.restore(&state));
        assert_eq!(other.u16_at(4), 0x6);
        let mut wrong = PciConfiguration::new(ids());
        assert!(!wrong.restore(&state));
    }
}
