use std::collections::VecDeque;
use std::io::Write;

use serde::{Deserialize, Serialize};

use super::Interrupt;
use super::bus::BusDevice;

const DATA: u64 = 0;
const IER: u64 = 1;
const IIR_FCR: u64 = 2;
const LCR: u64 = 3;
const MCR: u64 = 4;
const LSR: u64 = 5;
const MSR: u64 = 6;
const SCR: u64 = 7;

const IER_RDA: u8 = 0x01;
const IER_THRE: u8 = 0x02;
const IER_MASK: u8 = 0x0f;
const IIR_NONE: u8 = 0x01;
const IIR_THRE: u8 = 0x02;
const IIR_RDA: u8 = 0x04;
const IIR_FIFO_ENABLED: u8 = 0xc0;
const FCR_FIFO_ENABLE: u8 = 0x01;
const FCR_CLEAR_RX: u8 = 0x02;
const LCR_DLAB: u8 = 0x80;
const LSR_DATA_READY: u8 = 0x01;
const LSR_THR_EMPTY: u8 = 0x20;
const LSR_IDLE: u8 = 0x40;
const MCR_DTR: u8 = 0x01;
const MCR_RTS: u8 = 0x02;
const MCR_OUT1: u8 = 0x04;
const MCR_OUT2: u8 = 0x08;
const MCR_LOOP: u8 = 0x10;
const MCR_MASK: u8 = 0x1f;
const MSR_CTS: u8 = 0x10;
const MSR_DSR: u8 = 0x20;
const MSR_RI: u8 = 0x40;
const MSR_DCD: u8 = 0x80;
const DEFAULT_DIVISOR_LOW: u8 = 0x0c;
pub const INPUT_CAPACITY: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerialState {
    ier: u8,
    lcr: u8,
    mcr: u8,
    fcr: u8,
    scr: u8,
    dll: u8,
    dlm: u8,
    thre_pending: bool,
    input: VecDeque<u8>,
}

impl Default for SerialState {
    fn default() -> Self {
        Self {
            ier: 0,
            lcr: 0,
            mcr: MCR_OUT2,
            fcr: 0,
            scr: 0,
            dll: DEFAULT_DIVISOR_LOW,
            dlm: 0,
            thre_pending: false,
            input: VecDeque::new(),
        }
    }
}

pub struct Serial {
    state: SerialState,
    interrupt: Box<dyn Interrupt>,
    output: Box<dyn Write + Send>,
}

impl Serial {
    pub fn new(interrupt: Box<dyn Interrupt>, output: Box<dyn Write + Send>) -> Self {
        Self {
            state: SerialState::default(),
            interrupt,
            output,
        }
    }

    pub fn state(&self) -> SerialState {
        self.state.clone()
    }

    pub fn restore(&mut self, mut state: SerialState) {
        state.input.truncate(INPUT_CAPACITY);
        self.state = state;
    }

    pub fn input_room(&self) -> usize {
        INPUT_CAPACITY.saturating_sub(self.state.input.len())
    }

    pub fn enqueue_input(&mut self, bytes: &[u8]) -> usize {
        let accepted = bytes.len().min(self.input_room());
        if accepted == 0 {
            return 0;
        }
        self.state.input.extend(&bytes[..accepted]);
        if self.state.ier & IER_RDA != 0 {
            self.interrupt.trigger();
        }
        accepted
    }

    fn dlab(&self) -> bool {
        self.state.lcr & LCR_DLAB != 0
    }

    fn loopback(&self) -> bool {
        self.state.mcr & MCR_LOOP != 0
    }

    fn interrupt_identification(&self) -> u8 {
        let id = if self.state.ier & IER_RDA != 0 && !self.state.input.is_empty() {
            IIR_RDA
        } else if self.state.ier & IER_THRE != 0 && self.state.thre_pending {
            IIR_THRE
        } else {
            IIR_NONE
        };
        let fifo = if self.state.fcr & FCR_FIFO_ENABLE != 0 {
            IIR_FIFO_ENABLED
        } else {
            0
        };
        id | fifo
    }

    fn line_status(&self) -> u8 {
        let ready = if self.state.input.is_empty() {
            0
        } else {
            LSR_DATA_READY
        };
        ready | LSR_THR_EMPTY | LSR_IDLE
    }

    fn modem_status(&self) -> u8 {
        if !self.loopback() {
            return MSR_DCD | MSR_DSR | MSR_CTS;
        }
        let mcr = self.state.mcr;
        let mut msr = 0;
        for (mcr_bit, msr_bit) in [
            (MCR_RTS, MSR_CTS),
            (MCR_DTR, MSR_DSR),
            (MCR_OUT1, MSR_RI),
            (MCR_OUT2, MSR_DCD),
        ] {
            if mcr & mcr_bit != 0 {
                msr |= msr_bit;
            }
        }
        msr
    }

    fn transmit(&mut self, byte: u8) {
        if self.loopback() {
            if self.state.input.len() < INPUT_CAPACITY {
                self.state.input.push_back(byte);
            }
        } else {
            let _ = self.output.write_all(&[byte]);
            let _ = self.output.flush();
        }
        self.state.thre_pending = true;
        if self.state.ier & IER_THRE != 0 || (self.loopback() && self.state.ier & IER_RDA != 0) {
            self.interrupt.trigger();
        }
    }

    fn write_ier(&mut self, value: u8) {
        let old = self.state.ier;
        let new = value & IER_MASK;
        self.state.ier = new;
        let thre_enabled = new & IER_THRE != 0 && old & IER_THRE == 0;
        let rda_enabled = new & IER_RDA != 0 && old & IER_RDA == 0 && !self.state.input.is_empty();
        if thre_enabled {
            self.state.thre_pending = true;
        }
        if thre_enabled || rda_enabled {
            self.interrupt.trigger();
        }
    }

    fn read_register(&mut self, offset: u64) -> u8 {
        match offset {
            DATA if self.dlab() => self.state.dll,
            DATA => self.state.input.pop_front().unwrap_or(0),
            IER if self.dlab() => self.state.dlm,
            IER => self.state.ier,
            IIR_FCR => {
                let iir = self.interrupt_identification();
                if iir & 0x0f == IIR_THRE {
                    self.state.thre_pending = false;
                }
                iir
            }
            LCR => self.state.lcr,
            MCR => self.state.mcr,
            LSR => self.line_status(),
            MSR => self.modem_status(),
            SCR => self.state.scr,
            _ => 0xff,
        }
    }

    fn write_register(&mut self, offset: u64, value: u8) {
        match offset {
            DATA if self.dlab() => self.state.dll = value,
            DATA => self.transmit(value),
            IER if self.dlab() => self.state.dlm = value,
            IER => self.write_ier(value),
            IIR_FCR => {
                if value & FCR_CLEAR_RX != 0 {
                    self.state.input.clear();
                }
                self.state.fcr = value & FCR_FIFO_ENABLE;
            }
            LCR => self.state.lcr = value,
            MCR => self.state.mcr = value & MCR_MASK,
            SCR => self.state.scr = value,
            _ => {}
        }
    }
}

impl BusDevice for Serial {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        match data {
            [byte] => *byte = self.read_register(offset),
            _ => data.fill(0xff),
        }
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        if let [byte] = data {
            self.write_register(offset, *byte);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::devices::test_utils::CountingInterrupt;

    #[derive(Clone, Default)]
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBuffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn serial() -> (Serial, CountingInterrupt, SharedBuffer) {
        let irq = CountingInterrupt::default();
        let out = SharedBuffer::default();
        (
            Serial::new(Box::new(irq.clone()), Box::new(out.clone())),
            irq,
            out,
        )
    }

    fn read(serial: &mut Serial, offset: u64) -> u8 {
        let mut data = [0u8];
        serial.read(offset, &mut data);
        data[0]
    }

    #[test]
    fn transmits_and_raises_thre_interrupt() {
        let (mut serial, irq, out) = serial();
        serial.write(DATA, b"h");
        assert_eq!(irq.count(), 0);
        serial.write(IER, &[IER_THRE]);
        assert_eq!(irq.count(), 1);
        assert_eq!(read(&mut serial, IIR_FCR), IIR_THRE);
        assert_eq!(read(&mut serial, IIR_FCR), IIR_NONE);
        serial.write(DATA, b"i");
        assert_eq!(irq.count(), 2);
        assert_eq!(*out.0.lock().unwrap(), b"hi");
        assert_eq!(read(&mut serial, LSR), LSR_THR_EMPTY | LSR_IDLE);
    }

    #[test]
    fn receives_input_with_bounded_fifo() {
        let (mut serial, irq, _) = serial();
        serial.write(IER, &[IER_RDA]);
        assert_eq!(serial.enqueue_input(&[b'a'; 100]), INPUT_CAPACITY);
        assert_eq!(irq.count(), 1);
        assert_eq!(serial.input_room(), 0);
        assert_eq!(read(&mut serial, IIR_FCR) & 0x0f, IIR_RDA);
        assert_eq!(read(&mut serial, LSR) & LSR_DATA_READY, LSR_DATA_READY);
        for _ in 0..INPUT_CAPACITY {
            assert_eq!(read(&mut serial, DATA), b'a');
        }
        assert_eq!(read(&mut serial, LSR) & LSR_DATA_READY, 0);
        assert_eq!(read(&mut serial, DATA), 0);
    }

    #[test]
    fn divisor_latch_and_scratch() {
        let (mut serial, _, out) = serial();
        serial.write(LCR, &[LCR_DLAB]);
        serial.write(DATA, &[0x01]);
        serial.write(IER, &[0x02]);
        assert_eq!((read(&mut serial, DATA), read(&mut serial, IER)), (1, 2));
        serial.write(LCR, &[0x03]);
        assert_eq!(read(&mut serial, IER), 0);
        serial.write(SCR, &[0x5a]);
        assert_eq!(read(&mut serial, SCR), 0x5a);
        assert!(out.0.lock().unwrap().is_empty());
    }

    #[test]
    fn loopback_mirrors_modem_lines() {
        let (mut serial, _, out) = serial();
        serial.write(MCR, &[MCR_LOOP | MCR_OUT2 | MCR_RTS]);
        assert_eq!(read(&mut serial, MSR) & 0xf0, 0x90);
        serial.write(DATA, b"x");
        assert_eq!(read(&mut serial, DATA), b'x');
        assert!(out.0.lock().unwrap().is_empty());
        serial.write(MCR, &[MCR_OUT2]);
        assert_eq!(read(&mut serial, MSR), MSR_DCD | MSR_DSR | MSR_CTS);
    }

    #[test]
    fn fifo_control() {
        let (mut serial, _, _) = serial();
        serial.enqueue_input(b"abc");
        serial.write(IIR_FCR, &[FCR_FIFO_ENABLE | FCR_CLEAR_RX]);
        assert_eq!(read(&mut serial, IIR_FCR), IIR_FIFO_ENABLED | IIR_NONE);
        assert_eq!(read(&mut serial, LSR) & LSR_DATA_READY, 0);
    }

    #[test]
    fn enabling_rda_with_pending_input_interrupts() {
        let (mut serial, irq, _) = serial();
        serial.enqueue_input(b"z");
        assert_eq!(irq.count(), 0);
        serial.write(IER, &[IER_RDA]);
        assert_eq!(irq.count(), 1);
    }

    #[test]
    fn wide_accesses_are_ignored() {
        let (mut serial, _, out) = serial();
        let mut data = [0u8; 2];
        serial.read(DATA, &mut data);
        assert_eq!(data, [0xff, 0xff]);
        serial.write(DATA, b"ab");
        assert!(out.0.lock().unwrap().is_empty());
    }

    #[test]
    fn state_round_trip() {
        let (mut serial, _, _) = serial();
        serial.write(IER, &[IER_RDA]);
        serial.enqueue_input(b"q");
        let state = serial.state();
        let (mut other, _, _) = self::serial();
        other.restore(state.clone());
        assert_eq!(other.state(), state);
        assert_eq!(read(&mut other, DATA), b'q');
    }
}
