#![no_main]

mod common;

use std::io;
use std::sync::Arc;

use libfuzzer_sys::fuzz_target;
use minivmm::devices::acpi_pm::AcpiPm;
use minivmm::devices::bus::{Bus, BusDevice};
use minivmm::devices::i8042::I8042;
use minivmm::devices::pci::PciDevice;
use minivmm::devices::pci::root::PciConfigIo;
use minivmm::devices::serial::Serial;
use minivmm::devices::virtio::block::{Block, Disk};
use minivmm::devices::virtio::pci::VirtioPciState;
use minivmm::exit::ExitSignal;
use minivmm::snapshot::VmSnapshot;

use common::{Bytes, NoopInterrupt, memory_with, virtio_pci};

struct ZeroDisk;

impl Disk for ZeroDisk {
    fn read_at(&self, buf: &mut [u8], _: u64) -> io::Result<()> {
        buf.fill(0);
        Ok(())
    }

    fn write_at(&self, _: &[u8], _: u64) -> io::Result<()> {
        Ok(())
    }

    fn flush(&self) -> io::Result<()> {
        Ok(())
    }
}

fn exercise(device: &mut dyn BusDevice, offsets: impl Iterator<Item = u64>) {
    for offset in offsets {
        for len in [1usize, 2, 4] {
            let mut data = vec![0u8; len];
            device.read(offset, &mut data);
        }
    }
}

fn restore_legacy_devices(snapshot: &VmSnapshot) {
    let exit = Arc::new(ExitSignal::new().expect("exit signal"));
    let mut serial = Serial::new(Box::new(NoopInterrupt), Box::new(io::sink()));
    serial.restore(snapshot.serial.clone());
    exercise(&mut serial, 0..8);
    serial.write(0, &[b'x']);
    let mut i8042 = I8042::new(exit.clone());
    i8042.restore(snapshot.i8042.clone());
    exercise(&mut i8042, 0..5);
    let mut pm = AcpiPm::new(exit);
    pm.restore(snapshot.acpi_pm);
    exercise(&mut pm, 0..6);
    let mut pci = PciConfigIo::new(Arc::new(Bus::new()));
    if pci.restore(&snapshot.pci) {
        exercise(&mut pci, 0..8);
    }
}

fuzz_target!(|data: &[u8]| {
    let mut input = Bytes::new(data);
    let selector = input.u8();
    let seed_len = usize::from(input.u8()) * 64;
    let seed: Vec<u8> = (0..seed_len).map(|_| input.u8()).collect();
    let state = if selector & 1 == 0 {
        let Ok(snapshot) = postcard::from_bytes::<VmSnapshot>(input.rest()) else {
            return;
        };
        restore_legacy_devices(&snapshot);
        snapshot.block
    } else {
        postcard::from_bytes::<VirtioPciState<()>>(input.rest()).ok()
    };
    let Some(state) = state else {
        return;
    };
    let block = Block::new(Box::new(ZeroDisk), 64 * 512, selector & 2 != 0, b"fuzz");
    let mut device = virtio_pci(block, memory_with(&seed));
    if device.restore(state).is_err() {
        return;
    }
    device.process_queue(0);
    exercise(
        &mut device,
        (0..0x40)
            .chain(0x1000..0x1001)
            .chain(0x2000..0x2010)
            .chain(0x4000..0x4030)
            .chain(0x5000..0x5008),
    );
    for offset in (0..256).step_by(4) {
        let mut data = [0u8; 4];
        device.read_config(offset, &mut data);
    }
    device.write(0x14, &[0]);
});
