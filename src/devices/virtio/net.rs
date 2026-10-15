use std::io;
use std::os::fd::{AsRawFd, RawFd};

use log::{debug, warn};

use super::queue::{Queue, QueueError};
use super::{QueueMask, TYPE_NET, VirtioDevice, queue_bit, read_config_bytes};
use crate::memory::GuestMemory;
use crate::sys::tap::{Tap, VNET_HEADER_LEN};

pub const QUEUE_SIZE: u16 = 256;
pub const RX_QUEUE: usize = 0;
pub const TX_QUEUE: usize = 1;
const MAX_FRAME_LEN: usize = VNET_HEADER_LEN + 65536;
const NUM_BUFFERS_OFFSET: usize = 10;

const VIRTIO_NET_F_MAC: u64 = 1 << 5;
const VIRTIO_NET_F_STATUS: u64 = 1 << 16;
const LINK_UP: u16 = 1;
const CONFIG_LEN: usize = 8;

pub trait NetBackend: Send {
    fn read_frame(&mut self, buf: &mut [u8]) -> io::Result<usize>;
    fn write_frame(&mut self, frame: &[u8]) -> io::Result<usize>;
    fn raw_fd(&self) -> Option<RawFd>;
}

impl NetBackend for Tap {
    fn read_frame(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        Tap::read_frame(self, buf)
    }

    fn write_frame(&mut self, frame: &[u8]) -> io::Result<usize> {
        Tap::write_frame(self, frame)
    }

    fn raw_fd(&self) -> Option<RawFd> {
        Some(self.as_raw_fd())
    }
}

pub struct Net {
    backend: Box<dyn NetBackend>,
    mac: [u8; 6],
    pending_rx: Option<Vec<u8>>,
}

impl Net {
    pub fn new(backend: Box<dyn NetBackend>, mac: [u8; 6]) -> Self {
        Self {
            backend,
            mac,
            pending_rx: None,
        }
    }

    pub fn backend_fd(&self) -> Option<RawFd> {
        self.backend.raw_fd()
    }

    pub fn rx_blocked(&self) -> bool {
        self.pending_rx.is_some()
    }

    fn config(&self) -> [u8; CONFIG_LEN] {
        let mut config = [0u8; CONFIG_LEN];
        config[..6].copy_from_slice(&self.mac);
        config[6..8].copy_from_slice(&LINK_UP.to_le_bytes());
        config
    }

    fn next_frame(&mut self) -> Option<Vec<u8>> {
        if let Some(frame) = self.pending_rx.take() {
            return Some(frame);
        }
        let mut buf = vec![0u8; MAX_FRAME_LEN];
        match self.backend.read_frame(&mut buf) {
            Ok(len) if len >= VNET_HEADER_LEN => {
                buf.truncate(len);
                buf[NUM_BUFFERS_OFFSET..VNET_HEADER_LEN].copy_from_slice(&1u16.to_le_bytes());
                Some(buf)
            }
            Ok(_) => None,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => None,
            Err(err) => {
                warn!("tap read failed: {err}");
                None
            }
        }
    }

    pub fn receive(
        &mut self,
        mem: &GuestMemory,
        queues: &mut [Queue],
    ) -> Result<QueueMask, QueueError> {
        let Some(queue) = queues.get_mut(RX_QUEUE) else {
            return Ok(0);
        };
        let mut mask = 0;
        while let Some(frame) = self.next_frame() {
            let Some(chain) = queue.pop(mem)? else {
                self.pending_rx = Some(frame);
                break;
            };
            let mut writer = chain.writer();
            if writer.remaining() < frame.len() as u64 {
                debug!(
                    "dropping {} byte frame: receive buffer too small",
                    frame.len()
                );
                queue.undo_pop();
                continue;
            }
            let written = writer.write(mem, &frame)?;
            queue.add_used(mem, chain.head, written as u32)?;
            mask = queue_bit(RX_QUEUE);
        }
        Ok(mask)
    }

    fn transmit(
        &mut self,
        mem: &GuestMemory,
        queues: &mut [Queue],
    ) -> Result<QueueMask, QueueError> {
        let Some(queue) = queues.get_mut(TX_QUEUE) else {
            return Ok(0);
        };
        let mut mask = 0;
        let mut frame = vec![0u8; MAX_FRAME_LEN];
        while let Some(chain) = queue.pop(mem)? {
            let mut reader = chain.reader();
            let len = reader.remaining();
            if (VNET_HEADER_LEN as u64..=MAX_FRAME_LEN as u64).contains(&len) {
                let n = reader.read(mem, &mut frame[..len as usize])?;
                if let Err(err) = self.backend.write_frame(&frame[..n]) {
                    debug!("dropping transmitted frame: {err}");
                }
            } else {
                debug!("dropping malformed {len} byte frame from guest");
            }
            queue.add_used(mem, chain.head, 0)?;
            mask = queue_bit(TX_QUEUE);
        }
        Ok(mask)
    }
}

impl VirtioDevice for Net {
    type State = ();

    fn device_type(&self) -> u32 {
        TYPE_NET
    }

    fn pci_class(&self) -> (u8, u8) {
        (0x02, 0x00)
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &[QUEUE_SIZE, QUEUE_SIZE]
    }

    fn features(&self) -> u64 {
        VIRTIO_NET_F_MAC | VIRTIO_NET_F_STATUS
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        read_config_bytes(&self.config(), offset, data);
    }

    fn reset(&mut self) {
        self.pending_rx = None;
    }

    fn process_queue(
        &mut self,
        mem: &GuestMemory,
        queues: &mut [Queue],
        index: usize,
    ) -> Result<QueueMask, QueueError> {
        match index {
            RX_QUEUE => self.receive(mem, queues),
            TX_QUEUE => self.transmit(mem, queues),
            _ => Ok(0),
        }
    }

    fn state(&self) {}

    fn restore(&mut self, _: ()) {}
}

pub fn parse_mac(text: &str) -> Option<[u8; 6]> {
    let mut mac = [0u8; 6];
    let mut parts = text.split(':');
    for byte in mac.iter_mut() {
        *byte = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    parts.next().is_none().then_some(mac)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::devices::virtio::queue::test_utils::*;
    use crate::memory::GuestAddress;

    #[derive(Clone, Default)]
    struct Loopback {
        incoming: Arc<Mutex<VecDeque<Vec<u8>>>>,
        sent: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl NetBackend for Loopback {
        fn read_frame(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.incoming.lock().unwrap().pop_front() {
                Some(frame) => {
                    buf[..frame.len()].copy_from_slice(&frame);
                    Ok(frame.len())
                }
                None => Err(io::ErrorKind::WouldBlock.into()),
            }
        }

        fn write_frame(&mut self, frame: &[u8]) -> io::Result<usize> {
            self.sent.lock().unwrap().push(frame.to_vec());
            Ok(frame.len())
        }

        fn raw_fd(&self) -> Option<RawFd> {
            None
        }
    }

    fn setup() -> (Net, Loopback, GuestMemory, Vec<Queue>) {
        let backend = Loopback::default();
        let net = Net::new(Box::new(backend.clone()), [2, 0, 0, 0, 0, 1]);
        let mem = memory();
        let rx = ready_queue(16);
        let mut tx = ready_queue(16);
        tx.desc_table = GuestAddress(DESC_TABLE + 0x4000);
        tx.avail_ring = GuestAddress(AVAIL_RING + 0x4000);
        tx.used_ring = GuestAddress(USED_RING + 0x4000);
        (net, backend, mem, vec![rx, tx])
    }

    #[test]
    fn receives_frames_into_guest_buffers() {
        let (mut net, backend, mem, mut queues) = setup();
        let frame: Vec<u8> = (0..60u8).collect();
        backend.incoming.lock().unwrap().push_back(frame.clone());
        assert_eq!(net.receive(&mem, &mut queues).unwrap(), 0);
        assert!(net.rx_blocked());
        chain(&mem, 0, &[(DATA_AREA, 2048, true)]);
        publish(&mem, 0, 0, 1);
        assert_eq!(net.process_queue(&mem, &mut queues, RX_QUEUE).unwrap(), 1);
        assert!(!net.rx_blocked());
        assert_eq!(used(&mem, 0), (0, 60));
        let mut received = vec![0u8; 60];
        mem.read(GuestAddress(DATA_AREA), &mut received).unwrap();
        assert_eq!(&received[12..], &frame[12..]);
        assert_eq!(&received[10..12], &[1, 0]);
    }

    #[test]
    fn drops_frames_that_do_not_fit() {
        let (mut net, backend, mem, mut queues) = setup();
        backend.incoming.lock().unwrap().push_back(vec![0; 200]);
        backend.incoming.lock().unwrap().push_back(vec![7; 20]);
        chain(&mem, 0, &[(DATA_AREA, 64, true)]);
        publish(&mem, 0, 0, 1);
        net.receive(&mem, &mut queues).unwrap();
        assert_eq!(used(&mem, 0), (0, 20));
        assert_eq!(used_index(&mem), 1);
    }

    #[test]
    fn transmits_guest_frames() {
        let (mut net, backend, mem, mut queues) = setup();
        mem.write(GuestAddress(DATA_AREA), &[9u8; 12]).unwrap();
        mem.write(GuestAddress(DATA_AREA + 0x100), &[5u8; 50])
            .unwrap();
        let base = 0x4000;
        let desc = |i: u16, addr, len, flags, next| {
            mem.write_obj(GuestAddress(DESC_TABLE + base + u64::from(i) * 16), &addr)
                .unwrap();
            mem.write_obj(
                GuestAddress(DESC_TABLE + base + u64::from(i) * 16 + 8),
                &len,
            )
            .unwrap();
            mem.write_obj(
                GuestAddress(DESC_TABLE + base + u64::from(i) * 16 + 12),
                &flags,
            )
            .unwrap();
            mem.write_obj(
                GuestAddress(DESC_TABLE + base + u64::from(i) * 16 + 14),
                &next,
            )
            .unwrap();
        };
        desc(0, DATA_AREA, 12u32, 1u16, 1u16);
        desc(1, DATA_AREA + 0x100, 50u32, 0u16, 0u16);
        mem.write_obj(GuestAddress(AVAIL_RING + base + 4), &0u16)
            .unwrap();
        mem.write_obj(GuestAddress(AVAIL_RING + base + 2), &1u16)
            .unwrap();
        assert_eq!(net.process_queue(&mem, &mut queues, TX_QUEUE).unwrap(), 2);
        let sent = backend.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].len(), 62);
        assert_eq!(sent[0][12], 5);
    }

    #[test]
    fn config_space_has_mac_and_link() {
        let (net, _, _, _) = setup();
        let mut config = [0u8; 8];
        net.read_config(0, &mut config);
        assert_eq!(&config[..6], &[2, 0, 0, 0, 0, 1]);
        assert_eq!(u16::from_le_bytes([config[6], config[7]]), LINK_UP);
    }

    #[test]
    fn parses_mac_addresses() {
        assert_eq!(
            parse_mac("06:00:ac:10:00:02"),
            Some([6, 0, 0xac, 0x10, 0, 2])
        );
        assert_eq!(parse_mac("06:00:ac:10:00"), None);
        assert_eq!(parse_mac("06:00:ac:10:00:02:03"), None);
        assert_eq!(parse_mac("zz:00:ac:10:00:02"), None);
    }
}
