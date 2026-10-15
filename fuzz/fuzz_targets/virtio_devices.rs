#![no_main]

mod common;

use std::collections::VecDeque;
use std::io;
use std::os::fd::RawFd;
use std::sync::{Arc, Mutex};

use libfuzzer_sys::fuzz_target;
use minivmm::devices::bus::BusDevice;
use minivmm::devices::virtio::block::{Block, Disk};
use minivmm::devices::virtio::net::{Net, NetBackend};

use common::{Bytes, memory_with, virtio_pci};

const DISK_SIZE: usize = 64 * 512;

struct MemDisk(Mutex<Vec<u8>>);

impl Disk for MemDisk {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        let data = self.0.lock().unwrap();
        let start = usize::try_from(offset).map_err(|_| io::ErrorKind::InvalidInput)?;
        let slice = data
            .get(start..start + buf.len())
            .ok_or(io::ErrorKind::UnexpectedEof)?;
        buf.copy_from_slice(slice);
        Ok(())
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        let mut data = self.0.lock().unwrap();
        let start = usize::try_from(offset).map_err(|_| io::ErrorKind::InvalidInput)?;
        let slice = data
            .get_mut(start..start + buf.len())
            .ok_or(io::ErrorKind::UnexpectedEof)?;
        slice.copy_from_slice(buf);
        Ok(())
    }

    fn flush(&self) -> io::Result<()> {
        Ok(())
    }
}

struct Loopback(Arc<Mutex<VecDeque<Vec<u8>>>>);

impl NetBackend for Loopback {
    fn read_frame(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let frame = self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .ok_or(io::ErrorKind::WouldBlock)?;
        let len = frame.len().min(buf.len());
        buf[..len].copy_from_slice(&frame[..len]);
        Ok(len)
    }

    fn write_frame(&mut self, frame: &[u8]) -> io::Result<usize> {
        let mut queue = self.0.lock().unwrap();
        if queue.len() < 8 {
            queue.push_back(frame.to_vec());
        }
        Ok(frame.len())
    }

    fn raw_fd(&self) -> Option<RawFd> {
        None
    }
}

fn drive(device: &mut dyn BusDevice, input: &mut Bytes, mut process: impl FnMut(u8)) {
    while !input.is_empty() {
        let op = input.u8();
        let offset = u64::from(input.u16()) & 0x7fff;
        let len = [1usize, 2, 4, 8][usize::from(op >> 6)];
        let value = u64::from(input.u32()) | (u64::from(input.u32()) << 32);
        match op & 0x3 {
            0 => device.write(offset, &value.to_le_bytes()[..len]),
            1 => {
                let mut data = vec![0u8; len];
                device.read(offset, &mut data);
            }
            _ => process(op),
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let mut input = Bytes::new(data);
    let selector = input.u8();
    let memory_len = usize::from(input.u16());
    let memory_seed = (0..memory_len).map(|_| input.u8()).collect::<Vec<_>>();
    let memory = memory_with(&memory_seed);
    if selector % 2 == 0 {
        let disk = MemDisk(Mutex::new(vec![0xa5; DISK_SIZE]));
        let block = Block::new(Box::new(disk), DISK_SIZE as u64, selector & 2 != 0, b"fuzz");
        let mut device = virtio_pci(block, memory);
        let mut process = |_: u8| {};
        drive(&mut device, &mut input, &mut process);
        device.process_queue(0);
    } else {
        let frames = Arc::new(Mutex::new(VecDeque::from(vec![vec![0x42; 64]; 3])));
        let net = Net::new(Box::new(Loopback(frames)), [2, 0, 0, 0, 0, 1]);
        let mut device = virtio_pci(net, memory);
        drive(&mut device, &mut input, |_| {});
        device.process_queue(0);
        device.process_queue(1);
        device.process(|net, memory, queues| net.receive(memory, queues));
    }
});
