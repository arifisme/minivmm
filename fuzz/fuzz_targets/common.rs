#![allow(dead_code)]

use std::io;
use std::sync::Arc;

use minivmm::devices::Interrupt;
use minivmm::devices::pci::msix::{MsiRouter, Msix, MsixVector};
use minivmm::devices::virtio::VirtioDevice;
use minivmm::devices::virtio::pci::{NotifyRegistrar, VirtioPciDevice};
use minivmm::kvm::irq::MsiMessage;
use minivmm::memory::{GuestAddress, GuestMemory};
use vmm_sys_util::eventfd::{EFD_NONBLOCK, EventFd};

pub const MEMORY_SIZE: usize = 0x20000;

pub struct NoopInterrupt;

impl Interrupt for NoopInterrupt {
    fn trigger(&self) {}
}

pub struct NoopRouter;

impl MsiRouter for NoopRouter {
    fn set_route(&self, _: u32, _: Option<MsiMessage>) {}
}

pub struct NoopRegistrar;

impl NotifyRegistrar for NoopRegistrar {
    fn register(&self, _: &EventFd, _: u64) -> io::Result<()> {
        Ok(())
    }

    fn unregister(&self, _: &EventFd, _: u64) -> io::Result<()> {
        Ok(())
    }
}

pub fn memory_with(contents: &[u8]) -> Arc<GuestMemory> {
    let memory = GuestMemory::new(&[(GuestAddress(0), MEMORY_SIZE)]).expect("guest memory");
    let len = contents.len().min(MEMORY_SIZE);
    memory
        .write(GuestAddress(0), &contents[..len])
        .expect("seed guest memory");
    Arc::new(memory)
}

pub fn virtio_pci<D: VirtioDevice>(device: D, memory: Arc<GuestMemory>) -> VirtioPciDevice<D> {
    let queues = device.queue_max_sizes().len();
    let vectors = (0..=queues)
        .map(|i| MsixVector {
            gsi: 24 + i as u32,
            interrupt: Box::new(NoopInterrupt),
        })
        .collect();
    let msix = Msix::new(vectors, Arc::new(NoopRouter));
    let events = (0..queues)
        .map(|_| EventFd::new(EFD_NONBLOCK).expect("eventfd"))
        .collect();
    VirtioPciDevice::new(device, memory, msix, events, Arc::new(NoopRegistrar), 0xc000_0000)
        .expect("virtio device")
}

pub struct Bytes<'a> {
    data: &'a [u8],
}

impl<'a> Bytes<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data }
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn u8(&mut self) -> u8 {
        let (first, rest) = self.data.split_first().unwrap_or((&0, &[]));
        self.data = rest;
        *first
    }

    pub fn u16(&mut self) -> u16 {
        u16::from_le_bytes([self.u8(), self.u8()])
    }

    pub fn u32(&mut self) -> u32 {
        u32::from_le_bytes([self.u8(), self.u8(), self.u8(), self.u8()])
    }

    pub fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.data)
    }
}
