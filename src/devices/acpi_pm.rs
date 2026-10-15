use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::bus::BusDevice;
use super::read_register_bytes;
use crate::arch::acpi::SLEEP_TYPE_S5;
use crate::exit::{ExitReason, ExitSignal};

const STATUS: usize = 0;
const ENABLE: usize = 2;
const CONTROL: usize = 4;
const REGISTER_FILE_LEN: usize = 6;

const SCI_ENABLED: u16 = 1 << 0;
const SLEEP_ENABLE: u16 = 1 << 13;
const SLEEP_TYPE_SHIFT: u16 = 10;
const SLEEP_TYPE_MASK: u16 = 0x7;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpiPmState {
    status: u16,
    enable: u16,
    control: u16,
}

pub struct AcpiPm {
    exit: Arc<ExitSignal>,
    status: u16,
    enable: u16,
    control: u16,
}

impl AcpiPm {
    pub fn new(exit: Arc<ExitSignal>) -> Self {
        Self {
            exit,
            status: 0,
            enable: 0,
            // The FADT has no SMI command port, so the OS expects ACPI mode to be on already.
            control: SCI_ENABLED,
        }
    }

    pub fn state(&self) -> AcpiPmState {
        AcpiPmState {
            status: self.status,
            enable: self.enable,
            control: self.control,
        }
    }

    pub fn restore(&mut self, state: AcpiPmState) {
        self.status = state.status;
        self.enable = state.enable;
        self.control = state.control | SCI_ENABLED;
    }

    fn registers(&self) -> [u8; REGISTER_FILE_LEN] {
        let mut file = [0u8; REGISTER_FILE_LEN];
        file[STATUS..STATUS + 2].copy_from_slice(&self.status.to_le_bytes());
        file[ENABLE..ENABLE + 2].copy_from_slice(&self.enable.to_le_bytes());
        file[CONTROL..CONTROL + 2].copy_from_slice(&self.control.to_le_bytes());
        file
    }

    fn write_byte(&mut self, index: usize, value: u8) {
        let register_byte = |register: u16, high: bool| {
            let shift = if high { 8 } else { 0 };
            (register & !(0xff << shift)) | (u16::from(value) << shift)
        };
        match index {
            0 | 1 => self.status &= !(u16::from(value) << (8 * index)),
            2 | 3 => self.enable = register_byte(self.enable, index == 3),
            4 | 5 => self.control = register_byte(self.control, index == 5) | SCI_ENABLED,
            _ => {}
        }
    }

    fn check_sleep_request(&mut self) {
        if self.control & SLEEP_ENABLE == 0 {
            return;
        }
        self.control &= !SLEEP_ENABLE;
        let sleep_type = (self.control >> SLEEP_TYPE_SHIFT) & SLEEP_TYPE_MASK;
        if sleep_type == u16::from(SLEEP_TYPE_S5) {
            self.exit.request(ExitReason::PowerOff);
        }
    }
}

impl BusDevice for AcpiPm {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        read_register_bytes(&self.registers(), offset, data);
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        for (i, value) in data.iter().enumerate() {
            if let Some(index) = usize::try_from(offset).ok().and_then(|o| o.checked_add(i)) {
                self.write_byte(index, *value);
            }
        }
        self.check_sleep_request();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device() -> (AcpiPm, Arc<ExitSignal>) {
        let exit = Arc::new(ExitSignal::new().unwrap());
        (AcpiPm::new(exit.clone()), exit)
    }

    fn sleep_command(sleep_type: u16) -> [u8; 2] {
        ((sleep_type << SLEEP_TYPE_SHIFT) | SLEEP_ENABLE).to_le_bytes()
    }

    #[test]
    fn s5_sleep_powers_off() {
        let (mut pm, exit) = device();
        pm.write(4, &(5u16 << SLEEP_TYPE_SHIFT).to_le_bytes());
        assert_eq!(exit.reason(), None);
        pm.write(4, &sleep_command(5));
        assert_eq!(exit.reason(), Some(ExitReason::PowerOff));
    }

    #[test]
    fn other_sleep_states_are_ignored() {
        let (mut pm, exit) = device();
        pm.write(4, &sleep_command(3));
        assert_eq!(exit.reason(), None);
        let mut control = [0u8; 2];
        pm.read(4, &mut control);
        assert_eq!(u16::from_le_bytes(control) & SLEEP_ENABLE, 0);
    }

    #[test]
    fn sci_is_always_enabled() {
        let (mut pm, _) = device();
        pm.write(4, &[0, 0]);
        let mut control = [0u8; 2];
        pm.read(4, &mut control);
        assert_eq!(u16::from_le_bytes(control) & SCI_ENABLED, SCI_ENABLED);
    }

    #[test]
    fn state_round_trip() {
        let (mut pm, _) = device();
        pm.write(2, &[0x21, 0x01]);
        let (mut other, _) = device();
        other.restore(pm.state());
        assert_eq!(other.state(), pm.state());
    }

    #[test]
    fn status_is_write_one_to_clear() {
        let (mut pm, _) = device();
        pm.status = 0x8001;
        pm.write(0, &[0x01, 0x00]);
        let mut status = [0u8; 2];
        pm.read(0, &mut status);
        assert_eq!(u16::from_le_bytes(status), 0x8000);
        pm.write(2, &[0x20, 0x01, 0, 0]);
        let mut all = [0u8; 8];
        pm.read(0, &mut all);
        assert_eq!(&all[2..4], &[0x20, 0x01]);
        assert_eq!(&all[6..], &[0, 0]);
    }
}
