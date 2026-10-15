#![no_main]

mod common;

use std::sync::{Arc, Mutex};

use libfuzzer_sys::fuzz_target;
use minivmm::devices::acpi_pm::AcpiPm;
use minivmm::devices::bus::{Bus, BusDevice};
use minivmm::devices::i8042::I8042;
use minivmm::devices::pci::root::PciConfigIo;
use minivmm::devices::serial::Serial;
use minivmm::devices::virtio::block::Block;
use minivmm::exit::ExitSignal;

use common::{Bytes, NoopInterrupt, memory_with, virtio_pci};

fuzz_target!(|data: &[u8]| {
    let exit = Arc::new(ExitSignal::new().expect("exit signal"));
    let mut serial = Serial::new(Box::new(NoopInterrupt), Box::new(std::io::sink()));
    let mut i8042 = I8042::new(exit.clone());
    let mut pm = AcpiPm::new(exit);
    let mmio = Arc::new(Bus::new());
    let file = vmm_sys_util::tempfile::TempFile::new().expect("temp file");
    file.as_file().set_len(4096).expect("size temp file");
    let block = Block::open(file.as_path(), false).expect("block device");
    let device = Arc::new(Mutex::new(virtio_pci(block, memory_with(&[]))));
    mmio.insert(device.clone(), 0xc000_0000, 0x8000)
        .expect("map BAR");
    let mut pci = PciConfigIo::new(mmio.clone());
    pci.add_device(device);

    let mut input = Bytes::new(data);
    while !input.is_empty() {
        let op = input.u8();
        let offset = u64::from(input.u16());
        let len = [1usize, 2, 4, 8][usize::from((op >> 4) & 0x3)];
        let value = input.u32().to_le_bytes();
        let mut wide = [0u8; 8];
        wide[..4].copy_from_slice(&value);
        let target: &mut dyn BusDevice = match op & 0x7 {
            0 => &mut serial,
            1 => &mut i8042,
            2 => &mut pm,
            _ => &mut pci,
        };
        if op & 0x8 != 0 {
            target.write(offset % 16, &wide[..len]);
        } else {
            let mut out = vec![0u8; len];
            target.read(offset % 16, &mut out);
        }
        if op & 0x80 != 0 {
            serial.enqueue_input(&value);
            let mut out = [0u8; 4];
            mmio.read(0xc000_0000 + u64::from(input.u16()), &mut out);
            mmio.write(0xc000_0000 + u64::from(input.u16()), &value);
        }
    }
});
