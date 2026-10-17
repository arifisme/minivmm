pub mod block;
pub mod net;
pub mod pci;
pub mod queue;
pub mod vsock;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::memory::GuestMemory;
use queue::{Queue, QueueError};

pub const VIRTIO_F_VERSION_1: u64 = 1 << 32;
pub const TYPE_NET: u32 = 1;
pub const TYPE_BLOCK: u32 = 2;
pub const TYPE_VSOCK: u32 = 19;

pub type QueueMask = u64;

pub fn queue_bit(index: usize) -> QueueMask {
    1 << index
}

pub trait VirtioDevice: Send {
    type State: Serialize + DeserializeOwned;

    fn device_type(&self) -> u32;
    fn pci_class(&self) -> (u8, u8);
    fn queue_max_sizes(&self) -> &[u16];
    fn features(&self) -> u64;
    fn read_config(&self, offset: u64, data: &mut [u8]);
    fn write_config(&mut self, _offset: u64, _data: &[u8]) {}
    fn activate(&mut self, _features: u64) {}
    fn reset(&mut self) {}
    fn process_queue(
        &mut self,
        mem: &GuestMemory,
        queues: &mut [Queue],
        index: usize,
    ) -> Result<QueueMask, QueueError>;
    fn state(&self) -> Self::State;
    fn restore(&mut self, state: Self::State);
}

fn read_config_bytes(config: &[u8], offset: u64, data: &mut [u8]) {
    super::read_register_bytes(config, offset, data);
}
