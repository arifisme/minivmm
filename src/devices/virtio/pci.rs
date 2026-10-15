use std::io;
use std::sync::Arc;

use kvm_ioctls::{IoEventAddress, NoDatamatch, VmFd};
use log::{debug, warn};
use serde::{Deserialize, Serialize};
use vmm_sys_util::eventfd::EventFd;

use super::queue::{NO_VECTOR, Queue, QueueError};
use super::{QueueMask, VIRTIO_F_VERSION_1, VirtioDevice};
use crate::devices::bus::BusDevice;
use crate::devices::pci::PciDevice;
use crate::devices::pci::config::{BarChange, PciConfigState, PciConfiguration, PciIds};
use crate::devices::pci::msix::{MSIX_CAPABILITY_ID, Msix, MsixState};
use crate::memory::{GuestAddress, GuestMemory};

pub const VIRTIO_PCI_VENDOR: u16 = 0x1af4;
const VIRTIO_PCI_DEVICE_BASE: u16 = 0x1040;
const VIRTIO_PCI_SUBSYSTEM: u16 = 0x40;
const VIRTIO_PCI_REVISION: u8 = 1;

pub const BAR_SIZE: u32 = 0x8000;
const BAR_INDEX: u8 = 0;
const COMMON_CONFIG_OFFSET: u64 = 0x0000;
const COMMON_CONFIG_LEN: u64 = 0x38;
const ISR_OFFSET: u64 = 0x1000;
const DEVICE_CONFIG_OFFSET: u64 = 0x2000;
const DEVICE_CONFIG_LEN: u64 = 0x1000;
pub const NOTIFY_OFFSET: u64 = 0x3000;
pub const NOTIFY_MULTIPLIER: u32 = 4;
const MSIX_TABLE_OFFSET: u64 = 0x4000;
const MSIX_PBA_OFFSET: u64 = 0x5000;

const VENDOR_CAPABILITY_ID: u8 = 0x09;
const CAP_COMMON_CFG: u8 = 1;
const CAP_NOTIFY_CFG: u8 = 2;
const CAP_ISR_CFG: u8 = 3;
const CAP_DEVICE_CFG: u8 = 4;
const CAP_PCI_CFG: u8 = 5;
const PCI_CFG_CAP_BAR: usize = 4;
const PCI_CFG_CAP_OFFSET: usize = 8;
const PCI_CFG_CAP_LENGTH: usize = 12;
const PCI_CFG_CAP_DATA: usize = 16;
const MSIX_MESSAGE_CONTROL: usize = 2;

const STATUS_ACKNOWLEDGE: u8 = 1;
const STATUS_DRIVER: u8 = 2;
const STATUS_DRIVER_OK: u8 = 4;
const STATUS_FEATURES_OK: u8 = 8;
const STATUS_NEEDS_RESET: u8 = 0x40;
const STATUS_FAILED: u8 = 0x80;
const ISR_QUEUE: u8 = 1;
const ISR_CONFIG: u8 = 2;

pub trait NotifyRegistrar: Send + Sync {
    fn register(&self, event: &EventFd, addr: u64) -> io::Result<()>;
    fn unregister(&self, event: &EventFd, addr: u64) -> io::Result<()>;
}

impl NotifyRegistrar for VmFd {
    fn register(&self, event: &EventFd, addr: u64) -> io::Result<()> {
        self.register_ioevent(event, &IoEventAddress::Mmio(addr), NoDatamatch)
            .map_err(io::Error::from)
    }

    fn unregister(&self, event: &EventFd, addr: u64) -> io::Result<()> {
        self.unregister_ioevent(event, &IoEventAddress::Mmio(addr), NoDatamatch)
            .map_err(io::Error::from)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct CommonConfig {
    device_feature_select: u32,
    driver_feature_select: u32,
    driver_features: u64,
    msix_config: u16,
    status: u8,
    config_generation: u8,
    queue_select: u16,
}

#[derive(Serialize, Deserialize)]
pub struct VirtioPciState<S> {
    pci: PciConfigState,
    msix: MsixState,
    common: CommonConfig,
    queues: Vec<Queue>,
    isr: u8,
    device: S,
}

pub struct VirtioPciDevice<D: VirtioDevice> {
    device: D,
    config: PciConfiguration,
    msix: Msix,
    memory: Arc<GuestMemory>,
    common: CommonConfig,
    queues: Vec<Queue>,
    isr: u8,
    notify_events: Vec<EventFd>,
    registrar: Arc<dyn NotifyRegistrar>,
    msix_capability: usize,
    pci_cfg_capability: usize,
}

fn vendor_capability(cfg_type: u8, offset: u64, length: u64, extra: &[u8]) -> Vec<u8> {
    let mut body = vec![(16 + extra.len()) as u8, cfg_type, BAR_INDEX, 0, 0, 0];
    body.extend_from_slice(&(offset as u32).to_le_bytes());
    body.extend_from_slice(&(length as u32).to_le_bytes());
    body.extend_from_slice(extra);
    body
}

impl<D: VirtioDevice> VirtioPciDevice<D> {
    pub fn new(
        device: D,
        memory: Arc<GuestMemory>,
        msix: Msix,
        notify_events: Vec<EventFd>,
        registrar: Arc<dyn NotifyRegistrar>,
        bar_address: u32,
    ) -> io::Result<Self> {
        let queues: Vec<Queue> = device
            .queue_max_sizes()
            .iter()
            .map(|max| Queue::new(*max))
            .collect();
        if notify_events.len() != queues.len() || msix.vector_count() < queues.len() + 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "virtio device needs one notify event per queue and one vector per queue plus config",
            ));
        }
        let ids = PciIds {
            vendor: VIRTIO_PCI_VENDOR,
            device: VIRTIO_PCI_DEVICE_BASE + device.device_type() as u16,
            class: device.pci_class().0,
            subclass: device.pci_class().1,
            prog_if: 0,
            revision: VIRTIO_PCI_REVISION,
            subsystem_vendor: VIRTIO_PCI_VENDOR,
            subsystem: VIRTIO_PCI_SUBSYSTEM,
        };
        let mut config = PciConfiguration::new(ids);
        config.add_memory_bar(usize::from(BAR_INDEX), bar_address, BAR_SIZE);
        config.add_capability(
            VENDOR_CAPABILITY_ID,
            &vendor_capability(CAP_COMMON_CFG, COMMON_CONFIG_OFFSET, COMMON_CONFIG_LEN, &[]),
            &[],
        );
        config.add_capability(
            VENDOR_CAPABILITY_ID,
            &vendor_capability(
                CAP_NOTIFY_CFG,
                NOTIFY_OFFSET,
                u64::from(NOTIFY_MULTIPLIER) * queues.len() as u64,
                &NOTIFY_MULTIPLIER.to_le_bytes(),
            ),
            &[],
        );
        config.add_capability(
            VENDOR_CAPABILITY_ID,
            &vendor_capability(CAP_ISR_CFG, ISR_OFFSET, 1, &[]),
            &[],
        );
        config.add_capability(
            VENDOR_CAPABILITY_ID,
            &vendor_capability(CAP_DEVICE_CFG, DEVICE_CONFIG_OFFSET, DEVICE_CONFIG_LEN, &[]),
            &[],
        );
        let mut pci_cfg_writable = vec![0u8; 18];
        pci_cfg_writable[PCI_CFG_CAP_BAR - 2] = 0xff;
        for i in 0..4 {
            pci_cfg_writable[PCI_CFG_CAP_OFFSET - 2 + i] = 0xff;
            pci_cfg_writable[PCI_CFG_CAP_LENGTH - 2 + i] = 0xff;
            pci_cfg_writable[PCI_CFG_CAP_DATA - 2 + i] = 0xff;
        }
        let pci_cfg_capability = config.add_capability(
            VENDOR_CAPABILITY_ID,
            &vendor_capability(CAP_PCI_CFG, 0, 0, &[0; 4]),
            &pci_cfg_writable,
        );
        let (body, writable) =
            msix.capability_body(BAR_INDEX, MSIX_TABLE_OFFSET as u32, MSIX_PBA_OFFSET as u32);
        let msix_capability = config.add_capability(MSIX_CAPABILITY_ID, &body, &writable);

        let mut device = Self {
            device,
            config,
            msix,
            memory,
            common: CommonConfig::default(),
            queues,
            isr: 0,
            notify_events,
            registrar,
            msix_capability,
            pci_cfg_capability,
        };
        device.reset();
        device.register_notifiers(u64::from(bar_address))?;
        Ok(device)
    }

    pub fn device(&self) -> &D {
        &self.device
    }

    pub fn notify_events(&self) -> &[EventFd] {
        &self.notify_events
    }

    pub fn bar_address(&self) -> u64 {
        self.config.bar_address(usize::from(BAR_INDEX))
    }

    fn notify_address(bar: u64, queue: usize) -> u64 {
        bar + NOTIFY_OFFSET + u64::from(NOTIFY_MULTIPLIER) * queue as u64
    }

    fn register_notifiers(&self, bar: u64) -> io::Result<()> {
        for (queue, event) in self.notify_events.iter().enumerate() {
            self.registrar
                .register(event, Self::notify_address(bar, queue))?;
        }
        Ok(())
    }

    fn move_notifiers(&self, old: u64, new: u64) {
        for (queue, event) in self.notify_events.iter().enumerate() {
            if let Err(err) = self
                .registrar
                .unregister(event, Self::notify_address(old, queue))
            {
                warn!("failed to remove queue {queue} notifier: {err}");
            }
        }
        if let Err(err) = self.register_notifiers(new) {
            warn!("failed to install queue notifiers at {new:#x}: {err}");
        }
    }

    fn reset(&mut self) {
        self.device.reset();
        for queue in &mut self.queues {
            queue.reset();
        }
        self.common = CommonConfig {
            msix_config: NO_VECTOR,
            ..CommonConfig::default()
        };
        self.isr = 0;
    }

    fn device_features(&self) -> u64 {
        self.device.features() | VIRTIO_F_VERSION_1
    }

    pub fn is_active(&self) -> bool {
        let status = self.common.status;
        status & STATUS_DRIVER_OK != 0 && status & (STATUS_NEEDS_RESET | STATUS_FAILED) == 0
    }

    fn selected_queue(&self) -> Option<&Queue> {
        self.queues.get(usize::from(self.common.queue_select))
    }

    fn selected_queue_mut(&mut self) -> Option<&mut Queue> {
        self.queues.get_mut(usize::from(self.common.queue_select))
    }

    fn valid_vector(&self, vector: u16) -> u16 {
        if usize::from(vector) < self.msix.vector_count() {
            vector
        } else {
            NO_VECTOR
        }
    }

    fn read_common(&self, offset: u64, len: usize) -> u64 {
        let queue = self.selected_queue();
        let queue_field = |f: fn(&Queue) -> u64| queue.map_or(0, f);
        let select_half = |value: u64, select: u32| match select {
            0 => value & 0xffff_ffff,
            1 => value >> 32,
            _ => 0,
        };
        match (offset, len) {
            (0x00, 4) => u64::from(self.common.device_feature_select),
            (0x04, 4) => select_half(self.device_features(), self.common.device_feature_select),
            (0x08, 4) => u64::from(self.common.driver_feature_select),
            (0x0c, 4) => select_half(
                self.common.driver_features,
                self.common.driver_feature_select,
            ),
            (0x10, 2) => u64::from(self.common.msix_config),
            (0x12, 2) => self.queues.len() as u64,
            (0x14, 1) => u64::from(self.common.status),
            (0x15, 1) => u64::from(self.common.config_generation),
            (0x16, 2) => u64::from(self.common.queue_select),
            (0x18, 2) => queue_field(|q| u64::from(q.size)),
            (0x1a, 2) => queue.map_or(u64::from(NO_VECTOR), |q| u64::from(q.msix_vector)),
            (0x1c, 2) => queue_field(|q| u64::from(q.ready)),
            (0x1e, 2) => {
                if queue.is_some() {
                    u64::from(self.common.queue_select)
                } else {
                    0
                }
            }
            (0x20, 4) | (0x20, 8) => queue_field(|q| q.desc_table.raw()),
            (0x24, 4) => queue_field(|q| q.desc_table.raw() >> 32),
            (0x28, 4) | (0x28, 8) => queue_field(|q| q.avail_ring.raw()),
            (0x2c, 4) => queue_field(|q| q.avail_ring.raw() >> 32),
            (0x30, 4) | (0x30, 8) => queue_field(|q| q.used_ring.raw()),
            (0x34, 4) => queue_field(|q| q.used_ring.raw() >> 32),
            _ => 0,
        }
    }

    fn write_common(&mut self, offset: u64, len: usize, value: u64) {
        let features_locked = self.common.status & STATUS_FEATURES_OK != 0;
        match (offset, len) {
            (0x00, 4) => self.common.device_feature_select = value as u32,
            (0x08, 4) => self.common.driver_feature_select = value as u32,
            (0x0c, 4) if !features_locked => {
                let shift = match self.common.driver_feature_select {
                    0 => 0,
                    1 => 32,
                    _ => return,
                };
                let mask = 0xffff_ffffu64 << shift;
                self.common.driver_features =
                    (self.common.driver_features & !mask) | ((value << shift) & mask);
            }
            (0x10, 2) => self.common.msix_config = self.valid_vector(value as u16),
            (0x14, 1) => self.write_status(value as u8),
            (0x16, 2) => self.common.queue_select = value as u16,
            (0x18, 2) => {
                if let Some(queue) = self.selected_queue_mut().filter(|q| !q.ready) {
                    queue.size = value as u16;
                }
            }
            (0x1a, 2) => {
                let vector = self.valid_vector(value as u16);
                if let Some(queue) = self.selected_queue_mut() {
                    queue.msix_vector = vector;
                }
            }
            (0x1c, 2) if value == 1 => self.enable_selected_queue(),
            (0x20..=0x37, 4) | (0x20 | 0x28 | 0x30, 8) => {
                self.write_queue_address(offset, len, value)
            }
            _ => debug!("ignored common config write at {offset:#x}/{len}"),
        }
    }

    fn write_queue_address(&mut self, offset: u64, len: usize, value: u64) {
        let Some(queue) = self.selected_queue_mut().filter(|q| !q.ready) else {
            return;
        };
        let field = match offset & !0x7 {
            0x20 => &mut queue.desc_table,
            0x28 => &mut queue.avail_ring,
            _ => &mut queue.used_ring,
        };
        let (shift, mask) = match (offset % 8, len) {
            (0, 8) => (0, u64::MAX),
            (0, _) => (0, 0xffff_ffff),
            _ => (32, 0xffff_ffff_0000_0000),
        };
        *field = GuestAddress((field.raw() & !mask) | ((value << shift) & mask));
    }

    fn enable_selected_queue(&mut self) {
        let memory = self.memory.clone();
        if let Some(queue) = self.selected_queue_mut() {
            if queue.is_valid(&memory) {
                queue.ready = true;
            } else {
                warn!("driver enabled an invalid virtqueue");
            }
        }
    }

    fn write_status(&mut self, value: u8) {
        if value == 0 {
            self.reset();
            return;
        }
        let old = self.common.status;
        if value & old != old {
            warn!("driver tried to clear device status bits ({old:#x} -> {value:#x})");
            return;
        }
        let mut status = value;
        if status & STATUS_FEATURES_OK != 0 && old & STATUS_FEATURES_OK == 0 {
            let features = self.common.driver_features;
            let acceptable =
                features & !self.device_features() == 0 && features & VIRTIO_F_VERSION_1 != 0;
            if !acceptable {
                status &= !STATUS_FEATURES_OK;
            }
        }
        if status & STATUS_DRIVER_OK != 0 && old & STATUS_DRIVER_OK == 0 {
            let ready = status & STATUS_FEATURES_OK != 0
                && status & (STATUS_ACKNOWLEDGE | STATUS_DRIVER)
                    == STATUS_ACKNOWLEDGE | STATUS_DRIVER;
            if ready {
                self.device.activate(self.common.driver_features);
                for event in &self.notify_events {
                    let _ = event.write(1);
                }
            } else {
                status &= !STATUS_DRIVER_OK;
            }
        }
        self.common.status = status;
    }

    fn signal_used(&mut self, mask: QueueMask) {
        for (index, queue) in self.queues.iter().enumerate() {
            if mask & super::queue_bit(index) != 0 && queue.needs_notification(&self.memory) {
                self.isr |= ISR_QUEUE;
                if queue.msix_vector != NO_VECTOR {
                    self.msix.notify(queue.msix_vector);
                }
            }
        }
    }

    fn fail(&mut self, err: QueueError) {
        warn!(
            "virtio device type {} needs reset: {err}",
            self.device.device_type()
        );
        self.common.status |= STATUS_NEEDS_RESET;
        self.isr |= ISR_CONFIG;
        self.common.config_generation = self.common.config_generation.wrapping_add(1);
        if self.common.msix_config != NO_VECTOR {
            self.msix.notify(self.common.msix_config);
        }
    }

    pub fn process<F>(&mut self, work: F)
    where
        F: FnOnce(&mut D, &GuestMemory, &mut [Queue]) -> Result<QueueMask, QueueError>,
    {
        if !self.is_active() {
            return;
        }
        let memory = self.memory.clone();
        match work(&mut self.device, &memory, &mut self.queues) {
            Ok(mask) => self.signal_used(mask),
            Err(err) => self.fail(err),
        }
    }

    pub fn process_queue(&mut self, index: usize) {
        self.process(|device, memory, queues| device.process_queue(memory, queues, index));
    }

    fn pci_cfg_window(&self) -> Option<(u64, usize)> {
        let cap = self.pci_cfg_capability;
        let bar = self.config.byte(cap + PCI_CFG_CAP_BAR);
        let offset = u64::from(self.config.u32_at(cap + PCI_CFG_CAP_OFFSET));
        let length = self.config.u32_at(cap + PCI_CFG_CAP_LENGTH) as usize;
        let aligned = matches!(length, 1 | 2 | 4) && offset.is_multiple_of(length as u64);
        let inside = offset + length as u64 <= u64::from(BAR_SIZE);
        (bar == BAR_INDEX && aligned && inside).then_some((offset, length))
    }

    fn touches(offset: usize, len: usize, start: usize, field_len: usize) -> bool {
        offset < start + field_len && start < offset + len
    }

    pub fn state(&self) -> VirtioPciState<D::State> {
        VirtioPciState {
            pci: self.config.state(),
            msix: self.msix.state(),
            common: self.common.clone(),
            queues: self.queues.clone(),
            isr: self.isr,
            device: self.device.state(),
        }
    }

    pub fn restore(&mut self, state: VirtioPciState<D::State>) -> io::Result<Option<BarChange>> {
        let invalid = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_string());
        let old_bar = self.bar_address();
        let queues_match = state.queues.len() == self.queues.len()
            && state
                .queues
                .iter()
                .zip(&self.queues)
                .all(|(a, b)| a.max_size == b.max_size);
        if !queues_match {
            return Err(invalid("virtqueue layout mismatch"));
        }
        if !self.config.restore(&state.pci) {
            return Err(invalid("PCI configuration mismatch"));
        }
        if !self.msix.restore(state.msix) {
            return Err(invalid("MSI-X table mismatch"));
        }
        self.common = state.common;
        self.queues = state.queues;
        self.isr = state.isr;
        self.device.restore(state.device);
        if self.is_active() {
            self.device.activate(self.common.driver_features);
        }
        let new_bar = self.bar_address();
        if new_bar == old_bar {
            return Ok(None);
        }
        self.move_notifiers(old_bar, new_bar);
        Ok(Some(BarChange {
            index: usize::from(BAR_INDEX),
            old: old_bar,
            new: new_bar,
        }))
    }
}

fn to_value(data: &[u8]) -> u64 {
    let mut bytes = [0u8; 8];
    bytes[..data.len().min(8)].copy_from_slice(&data[..data.len().min(8)]);
    u64::from_le_bytes(bytes)
}

impl<D: VirtioDevice> BusDevice for VirtioPciDevice<D> {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        match offset {
            o if o < COMMON_CONFIG_LEN => {
                let value = self.read_common(o, data.len());
                let bytes = value.to_le_bytes();
                let len = data.len().min(8);
                data[..len].copy_from_slice(&bytes[..len]);
            }
            ISR_OFFSET => {
                data.fill(0);
                data[0] = std::mem::take(&mut self.isr);
            }
            o if (DEVICE_CONFIG_OFFSET..DEVICE_CONFIG_OFFSET + DEVICE_CONFIG_LEN).contains(&o) => {
                self.device.read_config(o - DEVICE_CONFIG_OFFSET, data)
            }
            o if (MSIX_TABLE_OFFSET..MSIX_TABLE_OFFSET + self.msix.table_size() as u64)
                .contains(&o) =>
            {
                self.msix.read_table(o - MSIX_TABLE_OFFSET, data)
            }
            o if (MSIX_PBA_OFFSET..MSIX_PBA_OFFSET + self.msix.pba_size() as u64).contains(&o) => {
                self.msix.read_pba(o - MSIX_PBA_OFFSET, data)
            }
            _ => data.fill(0),
        }
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        match offset {
            o if o < COMMON_CONFIG_LEN => self.write_common(o, data.len(), to_value(data)),
            o if (DEVICE_CONFIG_OFFSET..DEVICE_CONFIG_OFFSET + DEVICE_CONFIG_LEN).contains(&o) => {
                self.device.write_config(o - DEVICE_CONFIG_OFFSET, data)
            }
            o if (NOTIFY_OFFSET..NOTIFY_OFFSET + 4 * self.queues.len() as u64).contains(&o) => {
                let queue = ((o - NOTIFY_OFFSET) / u64::from(NOTIFY_MULTIPLIER)) as usize;
                if let Some(event) = self.notify_events.get(queue) {
                    let _ = event.write(1);
                }
            }
            o if (MSIX_TABLE_OFFSET..MSIX_TABLE_OFFSET + self.msix.table_size() as u64)
                .contains(&o) =>
            {
                self.msix.write_table(o - MSIX_TABLE_OFFSET, data)
            }
            _ => {}
        }
    }
}

impl<D: VirtioDevice> PciDevice for VirtioPciDevice<D> {
    fn read_config(&mut self, offset: usize, data: &mut [u8]) {
        self.config.read(offset, data);
        let window = self.pci_cfg_capability + PCI_CFG_CAP_DATA;
        if Self::touches(offset, data.len(), window, 4)
            && let Some((bar_offset, length)) = self.pci_cfg_window()
        {
            let mut value = [0u8; 4];
            BusDevice::read(self, bar_offset, &mut value[..length]);
            for (i, byte) in data.iter_mut().enumerate() {
                if let Some(index) = (offset + i).checked_sub(window).filter(|i| *i < 4) {
                    *byte = value[index];
                }
            }
        }
    }

    fn write_config(&mut self, offset: usize, data: &[u8]) -> Option<BarChange> {
        let change = self.config.write(offset, data);
        let window = self.pci_cfg_capability + PCI_CFG_CAP_DATA;
        if Self::touches(offset, data.len(), window, 4)
            && let Some((bar_offset, length)) = self.pci_cfg_window()
        {
            let value = self.config.u32_at(window).to_le_bytes();
            BusDevice::write(self, bar_offset, &value[..length]);
        }
        if Self::touches(
            offset,
            data.len(),
            self.msix_capability + MSIX_MESSAGE_CONTROL,
            2,
        ) {
            let control = self
                .config
                .u16_at(self.msix_capability + MSIX_MESSAGE_CONTROL);
            self.msix.set_message_control(control);
        }
        if let Some(change) = change {
            self.move_notifiers(change.old, change.new);
        }
        change
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::devices::pci::msix::{MsiRouter, MsixVector};
    use crate::devices::test_utils::CountingInterrupt;
    use crate::devices::virtio::queue::test_utils::*;
    use crate::kvm::irq::MsiMessage;

    pub struct NullRegistrar;

    impl NotifyRegistrar for NullRegistrar {
        fn register(&self, _: &EventFd, _: u64) -> io::Result<()> {
            Ok(())
        }

        fn unregister(&self, _: &EventFd, _: u64) -> io::Result<()> {
            Ok(())
        }
    }

    pub struct NullRouter;

    impl MsiRouter for NullRouter {
        fn set_route(&self, _: u32, _: Option<MsiMessage>) {}
    }

    struct Echo {
        processed: Arc<Mutex<Vec<usize>>>,
    }

    impl VirtioDevice for Echo {
        type State = ();

        fn device_type(&self) -> u32 {
            42
        }

        fn pci_class(&self) -> (u8, u8) {
            (0xff, 0)
        }

        fn queue_max_sizes(&self) -> &[u16] {
            &[16, 16]
        }

        fn features(&self) -> u64 {
            1 << 5
        }

        fn read_config(&self, offset: u64, data: &mut [u8]) {
            data.fill((offset as u8).wrapping_add(1));
        }

        fn process_queue(
            &mut self,
            mem: &GuestMemory,
            queues: &mut [Queue],
            index: usize,
        ) -> Result<QueueMask, QueueError> {
            self.processed.lock().unwrap().push(index);
            let queue = &mut queues[index];
            while let Some(chain) = queue.pop(mem)? {
                queue.add_used(mem, chain.head, 0)?;
            }
            Ok(super::super::queue_bit(index))
        }

        fn state(&self) {}

        fn restore(&mut self, _: ()) {}
    }

    pub fn build<D: VirtioDevice>(
        device: D,
        memory: Arc<GuestMemory>,
    ) -> (VirtioPciDevice<D>, Vec<CountingInterrupt>) {
        let queue_count = device.queue_max_sizes().len();
        let interrupts: Vec<CountingInterrupt> = (0..=queue_count)
            .map(|_| CountingInterrupt::default())
            .collect();
        let vectors = interrupts
            .iter()
            .enumerate()
            .map(|(i, irq)| MsixVector {
                gsi: 24 + i as u32,
                interrupt: Box::new(irq.clone()),
            })
            .collect();
        let msix = Msix::new(vectors, Arc::new(NullRouter));
        let events = (0..queue_count)
            .map(|_| EventFd::new(vmm_sys_util::eventfd::EFD_NONBLOCK).unwrap())
            .collect();
        let dev = VirtioPciDevice::new(
            device,
            memory,
            msix,
            events,
            Arc::new(NullRegistrar),
            0xc000_0000,
        )
        .unwrap();
        (dev, interrupts)
    }

    pub fn write16(dev: &mut impl BusDevice, offset: u64, value: u16) {
        dev.write(offset, &value.to_le_bytes());
    }

    pub fn write32(dev: &mut impl BusDevice, offset: u64, value: u32) {
        dev.write(offset, &value.to_le_bytes());
    }

    pub fn read32(dev: &mut impl BusDevice, offset: u64) -> u32 {
        let mut data = [0u8; 4];
        dev.read(offset, &mut data);
        u32::from_le_bytes(data)
    }

    pub fn read8(dev: &mut impl BusDevice, offset: u64) -> u8 {
        let mut data = [0u8; 1];
        dev.read(offset, &mut data);
        data[0]
    }

    pub fn enable_msix<D: VirtioDevice>(dev: &mut VirtioPciDevice<D>) {
        let control = dev.msix_capability + MSIX_MESSAGE_CONTROL;
        dev.write_config(control, &0x8000u16.to_le_bytes());
        for vector in 0..dev.msix.vector_count() as u64 {
            write32(dev, MSIX_TABLE_OFFSET + vector * 16 + 12, 0);
        }
    }

    pub fn setup_queue<D: VirtioDevice>(dev: &mut VirtioPciDevice<D>, index: u16, base: u64) {
        write16(dev, 0x16, index);
        write16(dev, 0x18, 16);
        write16(dev, 0x1a, index);
        write32(dev, 0x20, base as u32);
        write32(dev, 0x24, 0);
        write32(dev, 0x28, (base + 0x1000) as u32);
        write32(dev, 0x30, (base + 0x2000) as u32);
        write16(dev, 0x1c, 1);
    }

    pub fn negotiate<D: VirtioDevice>(dev: &mut VirtioPciDevice<D>, features: u64) {
        dev.write(0x14, &[STATUS_ACKNOWLEDGE]);
        dev.write(0x14, &[STATUS_ACKNOWLEDGE | STATUS_DRIVER]);
        write32(dev, 0x08, 0);
        write32(dev, 0x0c, features as u32);
        write32(dev, 0x08, 1);
        write32(dev, 0x0c, (features >> 32) as u32);
        dev.write(
            0x14,
            &[STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK],
        );
    }

    pub fn start<D: VirtioDevice>(dev: &mut VirtioPciDevice<D>) {
        dev.write(
            0x14,
            &[STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK],
        );
    }

    fn echo() -> (
        VirtioPciDevice<Echo>,
        Vec<CountingInterrupt>,
        Arc<GuestMemory>,
    ) {
        let memory = Arc::new(memory());
        let processed = Arc::new(Mutex::new(Vec::new()));
        let (dev, irqs) = build(Echo { processed }, memory.clone());
        (dev, irqs, memory)
    }

    #[test]
    fn exposes_modern_virtio_identity_and_capabilities() {
        let (mut dev, _, _) = echo();
        let mut id = [0u8; 4];
        dev.read_config(0, &mut id);
        assert_eq!(u32::from_le_bytes(id), 0x106a_1af4);
        let mut cap = dev.config.byte(0x34) as usize;
        let mut types = Vec::new();
        while cap != 0 {
            let id = dev.config.byte(cap);
            if id == VENDOR_CAPABILITY_ID {
                types.push(dev.config.byte(cap + 3));
            } else {
                types.push(id);
            }
            cap = dev.config.byte(cap + 1) as usize;
        }
        assert_eq!(types, vec![1, 2, 3, 4, 5, MSIX_CAPABILITY_ID]);
    }

    #[test]
    fn feature_negotiation() {
        let (mut dev, _, _) = echo();
        write32(&mut dev, 0x00, 1);
        assert_eq!(read32(&mut dev, 0x04), 1);
        write32(&mut dev, 0x00, 0);
        assert_eq!(read32(&mut dev, 0x04), 1 << 5);
        negotiate(&mut dev, VIRTIO_F_VERSION_1 | (1 << 7));
        assert_eq!(read8(&mut dev, 0x14) & STATUS_FEATURES_OK, 0);
        dev.write(0x14, &[0]);
        negotiate(&mut dev, VIRTIO_F_VERSION_1);
        assert_ne!(read8(&mut dev, 0x14) & STATUS_FEATURES_OK, 0);
        write32(&mut dev, 0x08, 0);
        write32(&mut dev, 0x0c, 1 << 5);
        write32(&mut dev, 0x08, 0);
        assert_eq!(
            read32(&mut dev, 0x0c),
            0,
            "features are locked after FEATURES_OK"
        );
    }

    #[test]
    fn rejects_legacy_drivers_and_status_regressions() {
        let (mut dev, _, _) = echo();
        negotiate(&mut dev, 1 << 5);
        assert_eq!(read8(&mut dev, 0x14) & STATUS_FEATURES_OK, 0);
        dev.write(
            0x14,
            &[STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_DRIVER_OK],
        );
        assert_eq!(read8(&mut dev, 0x14) & STATUS_DRIVER_OK, 0);
        dev.write(0x14, &[STATUS_ACKNOWLEDGE]);
        assert_eq!(read8(&mut dev, 0x14), STATUS_ACKNOWLEDGE | STATUS_DRIVER);
    }

    #[test]
    fn queue_configuration_and_processing() {
        let (mut dev, irqs, mem) = echo();
        enable_msix(&mut dev);
        negotiate(&mut dev, VIRTIO_F_VERSION_1);
        setup_queue(&mut dev, 1, DESC_TABLE);
        write16(&mut dev, 0x16, 1);
        assert_eq!(read32(&mut dev, 0x20), DESC_TABLE as u32);
        let mut size = [0u8; 2];
        dev.read(0x18, &mut size);
        assert_eq!(u16::from_le_bytes(size), 16);
        write32(&mut dev, 0x20, 0x9000);
        assert_eq!(
            read32(&mut dev, 0x20),
            DESC_TABLE as u32,
            "ready queue is frozen"
        );
        dev.process_queue(1);
        assert_eq!(irqs[1].count(), 0, "inactive device must not process");
        start(&mut dev);
        assert!(dev.is_active());
        assert_eq!(dev.notify_events()[1].read().unwrap(), 1);
        chain(&mem, 0, &[(DATA_AREA, 4, false)]);
        publish(&mem, 0, 0, 1);
        dev.process_queue(1);
        assert_eq!(used_index(&mem), 1);
        assert_eq!(irqs[1].count(), 1);
        assert_eq!(read8(&mut dev, ISR_OFFSET), ISR_QUEUE);
        assert_eq!(read8(&mut dev, ISR_OFFSET), 0);
    }

    #[test]
    fn invalid_queue_is_not_enabled() {
        let (mut dev, _, _) = echo();
        write16(&mut dev, 0x16, 0);
        write16(&mut dev, 0x18, 12);
        write16(&mut dev, 0x1c, 1);
        let mut ready = [0u8; 2];
        dev.read(0x1c, &mut ready);
        assert_eq!(ready, [0, 0]);
        write16(&mut dev, 0x16, 9);
        write16(&mut dev, 0x1c, 1);
        dev.read(0x18, &mut ready);
        assert_eq!(ready, [0, 0]);
    }

    #[test]
    fn queue_errors_request_reset() {
        let (mut dev, irqs, mem) = echo();
        enable_msix(&mut dev);
        negotiate(&mut dev, VIRTIO_F_VERSION_1);
        setup_queue(&mut dev, 0, DESC_TABLE);
        write16(&mut dev, 0x10, 2);
        start(&mut dev);
        publish(&mem, 0, 99, 1);
        dev.process_queue(0);
        assert_ne!(read8(&mut dev, 0x14) & STATUS_NEEDS_RESET, 0);
        assert_eq!(irqs[2].count(), 1);
        assert!(!dev.is_active());
        dev.write(0x14, &[0]);
        assert_eq!(read8(&mut dev, 0x14), 0);
        let mut vector = [0u8; 2];
        dev.read(0x10, &mut vector);
        assert_eq!(u16::from_le_bytes(vector), NO_VECTOR);
    }

    #[test]
    fn device_config_and_pci_cfg_window() {
        let (mut dev, _, _) = echo();
        assert_eq!(read8(&mut dev, DEVICE_CONFIG_OFFSET + 3), 4);
        let cap = dev.pci_cfg_capability;
        dev.write_config(cap + PCI_CFG_CAP_BAR, &[0]);
        dev.write_config(
            cap + PCI_CFG_CAP_OFFSET,
            &(DEVICE_CONFIG_OFFSET as u32 + 5).to_le_bytes(),
        );
        dev.write_config(cap + PCI_CFG_CAP_LENGTH, &1u32.to_le_bytes());
        let mut data = [0u8; 4];
        dev.read_config(cap + PCI_CFG_CAP_DATA, &mut data);
        assert_eq!(data[0], 6);
        dev.write_config(cap + PCI_CFG_CAP_OFFSET, &0x12u32.to_le_bytes());
        dev.write_config(cap + PCI_CFG_CAP_LENGTH, &2u32.to_le_bytes());
        dev.read_config(cap + PCI_CFG_CAP_DATA, &mut data);
        assert_eq!(u16::from_le_bytes([data[0], data[1]]), 2);
        dev.write_config(cap + PCI_CFG_CAP_OFFSET, &0x16u32.to_le_bytes());
        dev.write_config(cap + PCI_CFG_CAP_DATA, &1u32.to_le_bytes());
        let mut select = [0u8; 2];
        dev.read(0x16, &mut select);
        assert_eq!(u16::from_le_bytes(select), 1);
    }

    #[test]
    fn bar_relocation_reports_change() {
        let (mut dev, _, _) = echo();
        assert_eq!(dev.write_config(0x10, &[0xff; 4]), None);
        let change = dev.write_config(0x10, &0xd000_0000u32.to_le_bytes());
        assert_eq!(change.map(|c| c.new), Some(0xd000_0000));
        assert_eq!(dev.bar_address(), 0xd000_0000);
    }

    #[test]
    fn state_round_trip() {
        let (mut dev, _, mem) = echo();
        enable_msix(&mut dev);
        negotiate(&mut dev, VIRTIO_F_VERSION_1);
        setup_queue(&mut dev, 0, DESC_TABLE);
        start(&mut dev);
        chain(&mem, 0, &[(DATA_AREA, 4, false)]);
        publish(&mem, 0, 0, 1);
        dev.process_queue(0);
        let state = postcard::to_allocvec(&dev.state()).unwrap();
        let (mut other, irqs) = build(
            Echo {
                processed: Arc::new(Mutex::new(Vec::new())),
            },
            mem.clone(),
        );
        let restored = postcard::from_bytes(&state).unwrap();
        assert_eq!(other.restore(restored).unwrap(), None);
        assert!(other.is_active());
        chain(&mem, 1, &[(DATA_AREA, 4, false)]);
        publish(&mem, 1, 1, 2);
        other.process_queue(0);
        assert_eq!(used_index(&mem), 2);
        assert_eq!(irqs[0].count(), 1);
    }

    #[test]
    fn arbitrary_register_accesses_never_panic() {
        let (mut dev, _, _) = echo();
        for offset in (0..0x8000u64).step_by(3) {
            for len in [1usize, 2, 4, 8] {
                let mut data = vec![0u8; len];
                dev.read(offset, &mut data);
                dev.write(offset, &vec![0xa5; len]);
            }
        }
        for offset in 0..256usize {
            let mut data = [0u8; 4];
            dev.read_config(offset, &mut data[..(256 - offset).min(4)]);
            dev.write_config(offset, &[0x5a; 4][..(256 - offset).min(4)]);
        }
    }
}
