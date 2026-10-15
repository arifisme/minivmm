use std::sync::Arc;

use super::bus::BusDevice;
use crate::exit::{ExitReason, ExitSignal};

const DATA_OFFSET: u64 = 0;
const COMMAND_OFFSET: u64 = 4;

const STATUS_OUTPUT_FULL: u8 = 0x01;
const STATUS_SYSTEM_FLAG: u8 = 0x04;
const STATUS_COMMAND: u8 = 0x08;

const CMD_READ_COMMAND_BYTE: u8 = 0x20;
const CMD_WRITE_COMMAND_BYTE: u8 = 0x60;
const CMD_SELF_TEST: u8 = 0xaa;
const CMD_WRITE_OUTPUT_PORT: u8 = 0xd1;
const CMD_PULSE_RESET: u8 = 0xfe;
const SELF_TEST_PASSED: u8 = 0x55;
const OUTPUT_PORT_RESET_RELEASED: u8 = 0x01;

pub struct I8042 {
    exit: Arc<ExitSignal>,
    command_byte: u8,
    output: Option<u8>,
    pending_command: Option<u8>,
    last_was_command: bool,
}

impl I8042 {
    pub fn new(exit: Arc<ExitSignal>) -> Self {
        Self {
            exit,
            command_byte: 0,
            output: None,
            pending_command: None,
            last_was_command: false,
        }
    }

    fn status(&self) -> u8 {
        let mut status = STATUS_SYSTEM_FLAG;
        if self.output.is_some() {
            status |= STATUS_OUTPUT_FULL;
        }
        if self.last_was_command {
            status |= STATUS_COMMAND;
        }
        status
    }

    fn command(&mut self, command: u8) {
        self.last_was_command = true;
        match command {
            CMD_READ_COMMAND_BYTE => self.output = Some(self.command_byte),
            CMD_WRITE_COMMAND_BYTE | CMD_WRITE_OUTPUT_PORT => self.pending_command = Some(command),
            CMD_SELF_TEST => self.output = Some(SELF_TEST_PASSED),
            CMD_PULSE_RESET => self.exit.request(ExitReason::Reboot),
            _ => {}
        }
    }

    fn data(&mut self, value: u8) {
        self.last_was_command = false;
        match self.pending_command.take() {
            Some(CMD_WRITE_COMMAND_BYTE) => self.command_byte = value,
            Some(CMD_WRITE_OUTPUT_PORT) if value & OUTPUT_PORT_RESET_RELEASED == 0 => {
                self.exit.request(ExitReason::Reboot)
            }
            _ => {}
        }
    }
}

impl BusDevice for I8042 {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        let value = match (offset, data.len()) {
            (DATA_OFFSET, 1) => self.output.take().unwrap_or(0),
            (COMMAND_OFFSET, 1) => self.status(),
            _ => 0xff,
        };
        data.fill(value);
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        match (offset, data) {
            (DATA_OFFSET, [value]) => self.data(*value),
            (COMMAND_OFFSET, [value]) => self.command(*value),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device() -> (I8042, Arc<ExitSignal>) {
        let exit = Arc::new(ExitSignal::new().unwrap());
        (I8042::new(exit.clone()), exit)
    }

    #[test]
    fn pulse_reset_requests_reboot() {
        let (mut dev, exit) = device();
        let mut status = [0u8];
        dev.read(COMMAND_OFFSET, &mut status);
        assert_eq!(status[0] & 0x02, 0);
        dev.write(COMMAND_OFFSET, &[CMD_PULSE_RESET]);
        assert_eq!(exit.reason(), Some(ExitReason::Reboot));
    }

    #[test]
    fn output_port_reset() {
        let (mut dev, exit) = device();
        dev.write(COMMAND_OFFSET, &[CMD_WRITE_OUTPUT_PORT]);
        dev.write(DATA_OFFSET, &[0x01]);
        assert_eq!(exit.reason(), None);
        dev.write(COMMAND_OFFSET, &[CMD_WRITE_OUTPUT_PORT]);
        dev.write(DATA_OFFSET, &[0xfe]);
        assert_eq!(exit.reason(), Some(ExitReason::Reboot));
    }

    #[test]
    fn command_byte_and_self_test() {
        let (mut dev, exit) = device();
        dev.write(COMMAND_OFFSET, &[CMD_WRITE_COMMAND_BYTE]);
        dev.write(DATA_OFFSET, &[0x47]);
        dev.write(COMMAND_OFFSET, &[CMD_READ_COMMAND_BYTE]);
        let mut byte = [0u8];
        dev.read(COMMAND_OFFSET, &mut byte);
        assert_eq!(byte[0] & STATUS_OUTPUT_FULL, STATUS_OUTPUT_FULL);
        dev.read(DATA_OFFSET, &mut byte);
        assert_eq!(byte[0], 0x47);
        dev.write(COMMAND_OFFSET, &[CMD_SELF_TEST]);
        dev.read(DATA_OFFSET, &mut byte);
        assert_eq!(byte[0], SELF_TEST_PASSED);
        dev.read(DATA_OFFSET, &mut byte);
        assert_eq!(byte[0], 0);
        assert_eq!(exit.reason(), None);
    }

    #[test]
    fn unexpected_accesses_are_harmless() {
        let (mut dev, exit) = device();
        let mut wide = [0u8; 2];
        dev.read(1, &mut wide);
        assert_eq!(wide, [0xff, 0xff]);
        dev.write(COMMAND_OFFSET, &[CMD_PULSE_RESET, 0]);
        dev.write(2, &[CMD_PULSE_RESET]);
        assert_eq!(exit.reason(), None);
    }
}
