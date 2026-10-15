use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::Path;

use log::warn;

use super::queue::{ChainCursor, DescriptorChain, Queue, QueueError};
use super::{QueueMask, TYPE_BLOCK, VirtioDevice, queue_bit, read_config_bytes};
use crate::memory::GuestMemory;

pub const QUEUE_SIZE: u16 = 256;
const SECTOR_SIZE: u64 = 512;
const IO_CHUNK: usize = 128 * 1024;
const ID_LEN: usize = 20;
const HEADER_LEN: usize = 16;

const VIRTIO_BLK_F_SEG_MAX: u64 = 1 << 2;
const VIRTIO_BLK_F_RO: u64 = 1 << 5;
const VIRTIO_BLK_F_FLUSH: u64 = 1 << 9;

const REQUEST_IN: u32 = 0;
const REQUEST_OUT: u32 = 1;
const REQUEST_FLUSH: u32 = 4;
const REQUEST_GET_ID: u32 = 8;

const STATUS_OK: u8 = 0;
const STATUS_IOERR: u8 = 1;
const STATUS_UNSUPPORTED: u8 = 2;

const CONFIG_LEN: usize = 16;

pub trait Disk: Send {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;
    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()>;
    fn flush(&self) -> io::Result<()>;
}

impl Disk for File {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.read_exact_at(buf, offset)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        self.write_all_at(buf, offset)
    }

    fn flush(&self) -> io::Result<()> {
        self.sync_data()
    }
}

pub struct Block {
    disk: Box<dyn Disk>,
    sectors: u64,
    read_only: bool,
    id: [u8; ID_LEN],
}

enum Outcome {
    Status(u8),
    Data { status: u8, written: u32 },
}

impl Block {
    pub fn open(path: &Path, read_only: bool) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(!read_only).open(path)?;
        let metadata = file.metadata()?;
        let id = format!("minivmm-{:x}", metadata.ino());
        Ok(Self::new(
            Box::new(file),
            metadata.len(),
            read_only,
            id.as_bytes(),
        ))
    }

    pub fn new(disk: Box<dyn Disk>, size: u64, read_only: bool, id: &[u8]) -> Self {
        let mut serial = [0u8; ID_LEN];
        let len = id.len().min(ID_LEN);
        serial[..len].copy_from_slice(&id[..len]);
        Self {
            disk,
            sectors: size / SECTOR_SIZE,
            read_only,
            id: serial,
        }
    }

    fn config(&self) -> [u8; CONFIG_LEN] {
        let mut config = [0u8; CONFIG_LEN];
        config[0..8].copy_from_slice(&self.sectors.to_le_bytes());
        config[12..16].copy_from_slice(&u32::from(QUEUE_SIZE - 2).to_le_bytes());
        config
    }

    fn check_range(&self, sector: u64, len: u64) -> bool {
        len.is_multiple_of(SECTOR_SIZE)
            && sector
                .checked_add(len / SECTOR_SIZE)
                .is_some_and(|end| end <= self.sectors)
    }

    fn read_into(
        &self,
        mem: &GuestMemory,
        writer: &mut ChainCursor,
        sector: u64,
        len: u64,
    ) -> Outcome {
        if !self.check_range(sector, len) {
            return Outcome::Status(STATUS_IOERR);
        }
        let mut buf = vec![0u8; IO_CHUNK.min(len as usize)];
        let mut done = 0u64;
        while done < len {
            let n = (len - done).min(IO_CHUNK as u64) as usize;
            let offset = sector * SECTOR_SIZE + done;
            let copied = self.disk.read_at(&mut buf[..n], offset).is_ok()
                && writer.write(mem, &buf[..n]).is_ok_and(|count| count == n);
            if !copied {
                return Outcome::Data {
                    status: STATUS_IOERR,
                    written: done as u32,
                };
            }
            done += n as u64;
        }
        Outcome::Data {
            status: STATUS_OK,
            written: done as u32,
        }
    }

    fn write_from(&self, mem: &GuestMemory, reader: &mut ChainCursor, sector: u64) -> u8 {
        let len = reader.remaining();
        if self.read_only {
            return STATUS_IOERR;
        }
        if !self.check_range(sector, len) {
            return STATUS_IOERR;
        }
        let mut buf = vec![0u8; IO_CHUNK.min(len as usize)];
        let mut done = 0u64;
        while done < len {
            let n = (len - done).min(IO_CHUNK as u64) as usize;
            let ok = reader
                .read(mem, &mut buf[..n])
                .is_ok_and(|count| count == n)
                && self
                    .disk
                    .write_at(&buf[..n], sector * SECTOR_SIZE + done)
                    .is_ok();
            if !ok {
                return STATUS_IOERR;
            }
            done += n as u64;
        }
        STATUS_OK
    }

    fn execute(&self, mem: &GuestMemory, chain: &DescriptorChain) -> Option<u32> {
        let mut reader = chain.reader();
        let mut writer = chain.writer();
        let writable = writer.remaining();
        if writable == 0 {
            return None;
        }
        let data_room = writable - 1;
        let mut header = [0u8; HEADER_LEN];
        let outcome = match reader.read(mem, &mut header) {
            Ok(HEADER_LEN) => {
                let request = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
                let sector = u64::from_le_bytes(header[8..16].try_into().unwrap_or_default());
                match request {
                    REQUEST_IN => self.read_into(mem, &mut writer, sector, data_room),
                    REQUEST_OUT => Outcome::Status(self.write_from(mem, &mut reader, sector)),
                    REQUEST_FLUSH => Outcome::Status(match self.disk.flush() {
                        Ok(()) => STATUS_OK,
                        Err(err) => {
                            warn!("disk flush failed: {err}");
                            STATUS_IOERR
                        }
                    }),
                    REQUEST_GET_ID => {
                        let len = (data_room as usize).min(ID_LEN);
                        match writer.write(mem, &self.id[..len]) {
                            Ok(written) => Outcome::Data {
                                status: STATUS_OK,
                                written: written as u32,
                            },
                            Err(_) => Outcome::Status(STATUS_IOERR),
                        }
                    }
                    _ => Outcome::Status(STATUS_UNSUPPORTED),
                }
            }
            _ => Outcome::Status(STATUS_IOERR),
        };
        let (status, written) = match outcome {
            Outcome::Status(status) => (status, 0),
            Outcome::Data { status, written } => (status, written),
        };
        let mut status_writer = chain.writer();
        status_writer.skip(data_room);
        status_writer.write(mem, &[status]).ok()?;
        Some(written + 1)
    }
}

impl VirtioDevice for Block {
    type State = ();

    fn device_type(&self) -> u32 {
        TYPE_BLOCK
    }

    fn pci_class(&self) -> (u8, u8) {
        (0x01, 0x80)
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &[QUEUE_SIZE]
    }

    fn features(&self) -> u64 {
        let read_only = if self.read_only { VIRTIO_BLK_F_RO } else { 0 };
        VIRTIO_BLK_F_SEG_MAX | VIRTIO_BLK_F_FLUSH | read_only
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        read_config_bytes(&self.config(), offset, data);
    }

    fn process_queue(
        &mut self,
        mem: &GuestMemory,
        queues: &mut [Queue],
        index: usize,
    ) -> Result<QueueMask, QueueError> {
        let Some(queue) = queues.get_mut(index) else {
            return Ok(0);
        };
        let mut mask = 0;
        while let Some(chain) = queue.pop(mem)? {
            let len = self.execute(mem, &chain).unwrap_or(0);
            queue.add_used(mem, chain.head, len)?;
            mask = queue_bit(index);
        }
        Ok(mask)
    }

    fn state(&self) {}

    fn restore(&mut self, _: ()) {}
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::devices::virtio::VIRTIO_F_VERSION_1;
    use crate::devices::virtio::pci::tests::{build, enable_msix, negotiate, setup_queue, start};
    use crate::devices::virtio::queue::test_utils::*;
    use crate::memory::GuestAddress;

    #[derive(Clone)]
    struct MemDisk(Arc<Mutex<Vec<u8>>>, Arc<Mutex<u32>>);

    impl Disk for MemDisk {
        fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
            let data = self.0.lock().unwrap();
            let start = offset as usize;
            buf.copy_from_slice(&data[start..start + buf.len()]);
            Ok(())
        }

        fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
            let mut data = self.0.lock().unwrap();
            let start = offset as usize;
            data[start..start + buf.len()].copy_from_slice(buf);
            Ok(())
        }

        fn flush(&self) -> io::Result<()> {
            *self.1.lock().unwrap() += 1;
            Ok(())
        }
    }

    const HEADER: u64 = DATA_AREA;
    const BUFFER: u64 = DATA_AREA + 0x1000;
    const STATUS: u64 = DATA_AREA + 0x8000;

    struct Harness {
        disk: MemDisk,
        mem: Arc<GuestMemory>,
        queue: Queue,
        block: Block,
        next: u16,
    }

    fn harness(read_only: bool) -> Harness {
        let image: Vec<u8> = (0..8 * SECTOR_SIZE)
            .map(|i| (i / SECTOR_SIZE) as u8)
            .collect();
        let disk = MemDisk(Arc::new(Mutex::new(image)), Arc::new(Mutex::new(0)));
        Harness {
            block: Block::new(
                Box::new(disk.clone()),
                8 * SECTOR_SIZE,
                read_only,
                b"disk-id",
            ),
            disk,
            mem: Arc::new(memory()),
            queue: ready_queue(QUEUE_SIZE),
            next: 0,
        }
    }

    impl Harness {
        fn submit(&mut self, request: u32, sector: u64, data: Option<(u32, bool)>) -> (u8, u32) {
            let mut header = [0u8; 16];
            header[0..4].copy_from_slice(&request.to_le_bytes());
            header[8..16].copy_from_slice(&sector.to_le_bytes());
            self.mem.write(GuestAddress(HEADER), &header).unwrap();
            self.mem.write_obj(GuestAddress(STATUS), &0xffu8).unwrap();
            let mut buffers = vec![(HEADER, 16, false)];
            if let Some((len, writable)) = data {
                buffers.push((BUFFER, len, writable));
            }
            buffers.push((STATUS, 1, true));
            let head = self.next * 4;
            chain(&self.mem, head, &buffers);
            self.next += 1;
            publish(&self.mem, self.next - 1, head, self.next);
            let mask = self
                .block
                .process_queue(&self.mem, std::slice::from_mut(&mut self.queue), 0)
                .unwrap();
            assert_eq!(mask, 1);
            let status: u8 = self.mem.read_obj(GuestAddress(STATUS)).unwrap();
            (status, used(&self.mem, self.next - 1).1)
        }
    }

    #[test]
    fn reads_sectors() {
        let mut h = harness(false);
        assert_eq!(
            h.submit(REQUEST_IN, 2, Some((1024, true))),
            (STATUS_OK, 1025)
        );
        let mut data = [0u8; 1024];
        h.mem.read(GuestAddress(BUFFER), &mut data).unwrap();
        assert!(data[..512].iter().all(|b| *b == 2));
        assert!(data[512..].iter().all(|b| *b == 3));
    }

    #[test]
    fn writes_sectors_and_flushes() {
        let mut h = harness(false);
        h.mem.fill(GuestAddress(BUFFER), 512, 0xab).unwrap();
        assert_eq!(h.submit(REQUEST_OUT, 7, Some((512, false))), (STATUS_OK, 1));
        assert!(
            h.disk.0.lock().unwrap()[7 * 512..]
                .iter()
                .all(|b| *b == 0xab)
        );
        assert_eq!(h.submit(REQUEST_FLUSH, 0, None), (STATUS_OK, 1));
        assert_eq!(*h.disk.1.lock().unwrap(), 1);
    }

    #[test]
    fn rejects_out_of_range_and_unaligned_requests() {
        let mut h = harness(false);
        assert_eq!(h.submit(REQUEST_IN, 7, Some((1024, true))).0, STATUS_IOERR);
        assert_eq!(
            h.submit(REQUEST_IN, u64::MAX, Some((512, true))).0,
            STATUS_IOERR
        );
        assert_eq!(h.submit(REQUEST_OUT, 0, Some((100, false))).0, STATUS_IOERR);
        assert_eq!(h.submit(99, 0, None).0, STATUS_UNSUPPORTED);
    }

    #[test]
    fn read_only_disk_refuses_writes() {
        let mut h = harness(true);
        assert_ne!(h.block.features() & VIRTIO_BLK_F_RO, 0);
        h.mem.fill(GuestAddress(BUFFER), 512, 0xab).unwrap();
        assert_eq!(h.submit(REQUEST_OUT, 0, Some((512, false))).0, STATUS_IOERR);
        assert!(h.disk.0.lock().unwrap()[..512].iter().all(|b| *b == 0));
        assert_eq!(h.submit(REQUEST_IN, 1, Some((512, true))).0, STATUS_OK);
    }

    #[test]
    fn returns_device_id() {
        let mut h = harness(false);
        assert_eq!(
            h.submit(REQUEST_GET_ID, 0, Some((20, true))),
            (STATUS_OK, 21)
        );
        let mut id = [0u8; 7];
        h.mem.read(GuestAddress(BUFFER), &mut id).unwrap();
        assert_eq!(&id, b"disk-id");
    }

    #[test]
    fn malformed_requests_are_completed_safely() {
        let mut h = harness(false);
        chain(&h.mem, 0, &[(HEADER, 8, false), (STATUS, 1, true)]);
        publish(&h.mem, 0, 0, 1);
        h.block
            .process_queue(&h.mem, std::slice::from_mut(&mut h.queue), 0)
            .unwrap();
        assert_eq!(
            h.mem.read_obj::<u8>(GuestAddress(STATUS)).unwrap(),
            STATUS_IOERR
        );
        chain(&h.mem, 4, &[(HEADER, 16, false)]);
        publish(&h.mem, 1, 4, 2);
        h.block
            .process_queue(&h.mem, std::slice::from_mut(&mut h.queue), 0)
            .unwrap();
        assert_eq!(used(&h.mem, 1), (4, 0));
    }

    #[test]
    fn config_space_reports_capacity() {
        let h = harness(false);
        let mut capacity = [0u8; 8];
        h.block.read_config(0, &mut capacity);
        assert_eq!(u64::from_le_bytes(capacity), 8);
        let mut seg_max = [0u8; 4];
        h.block.read_config(12, &mut seg_max);
        assert_eq!(u32::from_le_bytes(seg_max), 254);
        let mut beyond = [0xffu8; 4];
        h.block.read_config(100, &mut beyond);
        assert_eq!(beyond, [0; 4]);
    }

    #[test]
    fn works_through_the_pci_transport() {
        let h = harness(false);
        let (mut dev, irqs) = build(h.block, h.mem.clone());
        enable_msix(&mut dev);
        negotiate(&mut dev, VIRTIO_F_VERSION_1 | VIRTIO_BLK_F_FLUSH);
        setup_queue(&mut dev, 0, DESC_TABLE);
        start(&mut dev);
        let mut header = [0u8; 16];
        header[8] = 1;
        h.mem.write(GuestAddress(HEADER), &header).unwrap();
        chain(
            &h.mem,
            0,
            &[(HEADER, 16, false), (BUFFER, 512, true), (STATUS, 1, true)],
        );
        publish(&h.mem, 0, 0, 1);
        dev.process_queue(0);
        assert_eq!(irqs[0].count(), 1);
        assert_eq!(h.mem.read_obj::<u8>(GuestAddress(BUFFER)).unwrap(), 1);
    }
}
