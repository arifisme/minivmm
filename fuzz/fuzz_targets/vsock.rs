#![no_main]

mod common;

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::fd::RawFd;
use std::sync::{Arc, Mutex};

use libfuzzer_sys::fuzz_target;
use minivmm::devices::bus::BusDevice;
use minivmm::devices::virtio::vsock::connection::VsockStream;
use minivmm::devices::virtio::vsock::{Vsock, VsockBackend};

use common::{Bytes, memory_with, virtio_pci};

const MAX_HOST_BYTES: usize = 1 << 20;

#[derive(Default)]
struct Pipe {
    incoming: VecDeque<u8>,
    written: usize,
    write_room: usize,
    eof: bool,
}

#[derive(Clone, Default)]
struct HostStream(Arc<Mutex<Pipe>>);

impl Read for HostStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut pipe = self.0.lock().unwrap();
        if pipe.incoming.is_empty() {
            return if pipe.eof {
                Ok(0)
            } else {
                Err(io::ErrorKind::WouldBlock.into())
            };
        }
        let n = buf.len().min(pipe.incoming.len());
        for byte in &mut buf[..n] {
            *byte = pipe.incoming.pop_front().unwrap_or(0);
        }
        Ok(n)
    }
}

impl Write for HostStream {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let mut pipe = self.0.lock().unwrap();
        let n = data.len().min(pipe.write_room);
        if n == 0 && !data.is_empty() {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        pipe.write_room -= n;
        pipe.written += n;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl VsockStream for HostStream {
    fn raw_fd(&self) -> Option<RawFd> {
        None
    }

    fn shutdown_write(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Clone, Default)]
struct Backend(Arc<Mutex<Vec<HostStream>>>);

impl VsockBackend for Backend {
    fn connect(&mut self, port: u32) -> io::Result<Box<dyn VsockStream>> {
        if port % 2 == 1 {
            return Err(io::ErrorKind::ConnectionRefused.into());
        }
        let stream = HostStream::default();
        self.0.lock().unwrap().push(stream.clone());
        Ok(Box::new(stream))
    }
}

fn write(device: &mut dyn BusDevice, offset: u64, value: u64, len: usize) {
    device.write(offset, &value.to_le_bytes()[..len]);
}

fn start_device(device: &mut dyn BusDevice) {
    write(device, 0x14, 1, 1);
    write(device, 0x14, 3, 1);
    write(device, 0x08, 1, 4);
    write(device, 0x0c, 1, 4);
    write(device, 0x14, 11, 1);
    for queue in 0..3u64 {
        let base = 0x1000 + queue * 0x1000;
        write(device, 0x16, queue, 2);
        write(device, 0x18, 16, 2);
        write(device, 0x20, base, 8);
        write(device, 0x28, base + 0x400, 8);
        write(device, 0x30, base + 0x800, 8);
        write(device, 0x1c, 1, 2);
    }
    write(device, 0x14, 15, 1);
}

fuzz_target!(|data: &[u8]| {
    let mut input = Bytes::new(data);
    let memory_len = usize::from(input.u16());
    let memory_seed = (0..memory_len).map(|_| input.u8()).collect::<Vec<_>>();
    let memory = memory_with(&memory_seed);
    let backend = Backend::default();
    let mut device = virtio_pci(Vsock::new(3, Box::new(backend.clone())), memory);
    if memory_len % 2 == 1 {
        start_device(&mut device);
    }
    let mut streams: Vec<HostStream> = Vec::new();
    let mut host_bytes = 0usize;
    while !input.is_empty() {
        let op = input.u8();
        match op % 8 {
            0 => {
                let offset = u64::from(input.u16()) & 0x7fff;
                let len = [1usize, 2, 4, 8][usize::from(op >> 6)];
                let value = u64::from(input.u32()) | (u64::from(input.u32()) << 32);
                device.write(offset, &value.to_le_bytes()[..len]);
            }
            1 => {
                let offset = u64::from(input.u16()) & 0x7fff;
                let mut buf = [0u8; 8];
                device.read(offset, &mut buf[..[1usize, 2, 4, 8][usize::from(op >> 6)]]);
            }
            2 => device.process_queue(usize::from(op >> 3) % 4),
            3 => {
                let stream = HostStream::default();
                streams.push(stream.clone());
                let port = input.u32();
                let early = input.u8();
                device.process(|vsock, memory, queues| {
                    vsock.connect_from_host(
                        Box::new(stream),
                        port,
                        vec![early; usize::from(early % 4)],
                    );
                    vsock.fill_rx(memory, queues)
                });
            }
            4 => {
                let id = u32::from(input.u8() % 8);
                device.process(|vsock, memory, queues| {
                    vsock.host_ready(id);
                    vsock.fill_rx(memory, queues)
                });
            }
            5 => {
                let len = usize::from(input.u16()) % 8192;
                let target = usize::from(input.u8());
                let all: Vec<HostStream> = streams
                    .iter()
                    .chain(backend.0.lock().unwrap().iter())
                    .cloned()
                    .collect();
                if let Some(stream) = all.get(target % all.len().max(1))
                    && host_bytes + len <= MAX_HOST_BYTES
                {
                    host_bytes += len;
                    stream
                        .0
                        .lock()
                        .unwrap()
                        .incoming
                        .extend(std::iter::repeat_n(op, len));
                }
            }
            6 => {
                let target = usize::from(input.u8());
                let room = usize::from(input.u16());
                if let Some(stream) = streams.get(target % streams.len().max(1)) {
                    let mut pipe = stream.0.lock().unwrap();
                    pipe.write_room = room;
                    pipe.eof = op & 0x80 != 0;
                }
            }
            _ => {
                let interests = device.device().interests();
                for interest in interests {
                    device.process(|vsock, memory, queues| {
                        vsock.host_ready(interest.id);
                        vsock.fill_rx(memory, queues)
                    });
                }
            }
        }
    }
});
