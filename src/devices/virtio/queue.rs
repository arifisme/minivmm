use std::sync::atomic::{Ordering, fence};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::memory::{GuestAddress, GuestMemory, MemoryError, Pod};

pub const VIRTQ_DESC_F_NEXT: u16 = 1;
pub const VIRTQ_DESC_F_WRITE: u16 = 2;
pub const VIRTQ_DESC_F_INDIRECT: u16 = 4;
const VIRTQ_AVAIL_F_NO_INTERRUPT: u16 = 1;
const DESCRIPTOR_SIZE: u64 = 16;
const RING_INDEX_OFFSET: u64 = 2;
const RING_ENTRIES_OFFSET: u64 = 4;
const AVAIL_ENTRY_SIZE: u64 = 2;
const USED_ENTRY_SIZE: u64 = 8;
const RING_TRAILER_SIZE: u64 = 2;
pub const NO_VECTOR: u16 = 0xffff;

#[derive(Debug, Error)]
pub enum QueueError {
    #[error("descriptor index {0} is outside the queue")]
    DescriptorOutOfRange(u16),
    #[error("descriptor chain is longer than the queue")]
    ChainTooLong,
    #[error("indirect descriptors were not negotiated")]
    IndirectDescriptor,
    #[error("descriptor buffer {addr:#x}+{len:#x} is outside guest memory")]
    InvalidBuffer { addr: u64, len: u32 },
    #[error("driver published {0} buffers, more than the queue holds")]
    AvailIndexAhead(u16),
    #[error("queue is not usable")]
    NotReady,
    #[error(transparent)]
    Memory(#[from] MemoryError),
}

pub type Result<T> = std::result::Result<T, QueueError>;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct RawDescriptor {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

// SAFETY: four integer fields with no padding (8 + 4 + 2 + 2 bytes).
unsafe impl Pod for RawDescriptor {}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct UsedElement {
    id: u32,
    len: u32,
}

// SAFETY: two u32 fields with no padding.
unsafe impl Pod for UsedElement {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Descriptor {
    pub addr: GuestAddress,
    pub len: u32,
    pub writable: bool,
}

#[derive(Clone, Debug)]
pub struct DescriptorChain {
    pub head: u16,
    descriptors: Vec<Descriptor>,
}

impl DescriptorChain {
    fn parse(mem: &GuestMemory, table: GuestAddress, size: u16, head: u16) -> Result<Self> {
        let mut descriptors = Vec::new();
        let mut index = head;
        loop {
            if index >= size {
                return Err(QueueError::DescriptorOutOfRange(index));
            }
            if descriptors.len() >= usize::from(size) {
                return Err(QueueError::ChainTooLong);
            }
            let raw: RawDescriptor = mem.read_obj(GuestAddress(
                table.raw() + u64::from(index) * DESCRIPTOR_SIZE,
            ))?;
            if raw.flags & VIRTQ_DESC_F_INDIRECT != 0 {
                return Err(QueueError::IndirectDescriptor);
            }
            if raw.len > 0 && !mem.contains_range(GuestAddress(raw.addr), raw.len as usize) {
                return Err(QueueError::InvalidBuffer {
                    addr: raw.addr,
                    len: raw.len,
                });
            }
            descriptors.push(Descriptor {
                addr: GuestAddress(raw.addr),
                len: raw.len,
                writable: raw.flags & VIRTQ_DESC_F_WRITE != 0,
            });
            if raw.flags & VIRTQ_DESC_F_NEXT == 0 {
                break;
            }
            index = raw.next;
        }
        Ok(Self { head, descriptors })
    }

    pub fn descriptors(&self) -> &[Descriptor] {
        &self.descriptors
    }

    pub fn reader(&self) -> ChainCursor {
        ChainCursor::new(self.descriptors.iter().filter(|d| !d.writable))
    }

    pub fn writer(&self) -> ChainCursor {
        ChainCursor::new(self.descriptors.iter().filter(|d| d.writable))
    }
}

#[derive(Clone, Debug)]
pub struct ChainCursor {
    segments: Vec<(GuestAddress, u64)>,
    current: usize,
    offset: u64,
}

impl ChainCursor {
    fn new<'a>(descriptors: impl Iterator<Item = &'a Descriptor>) -> Self {
        Self {
            segments: descriptors
                .filter(|d| d.len > 0)
                .map(|d| (d.addr, u64::from(d.len)))
                .collect(),
            current: 0,
            offset: 0,
        }
    }

    pub fn remaining(&self) -> u64 {
        let total: u64 = self.segments[self.current.min(self.segments.len())..]
            .iter()
            .map(|(_, len)| len)
            .sum();
        total - self.offset
    }

    fn next_segment(&mut self, max: usize) -> Option<(GuestAddress, usize)> {
        while let Some(&(addr, len)) = self.segments.get(self.current) {
            if self.offset < len {
                let count = (len - self.offset).min(max as u64) as usize;
                let at = GuestAddress(addr.raw() + self.offset);
                self.offset += count as u64;
                return Some((at, count));
            }
            self.current += 1;
            self.offset = 0;
        }
        None
    }

    pub fn read(&mut self, mem: &GuestMemory, buf: &mut [u8]) -> Result<usize> {
        let mut done = 0;
        while done < buf.len() {
            let Some((addr, count)) = self.next_segment(buf.len() - done) else {
                break;
            };
            mem.read(addr, &mut buf[done..done + count])?;
            done += count;
        }
        Ok(done)
    }

    pub fn write(&mut self, mem: &GuestMemory, data: &[u8]) -> Result<usize> {
        let mut done = 0;
        while done < data.len() {
            let Some((addr, count)) = self.next_segment(data.len() - done) else {
                break;
            };
            mem.write(addr, &data[done..done + count])?;
            done += count;
        }
        Ok(done)
    }

    pub fn skip(&mut self, mut count: u64) {
        while count > 0 {
            let Some((_, n)) = self.next_segment(count.min(usize::MAX as u64) as usize) else {
                break;
            };
            count -= n as u64;
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Queue {
    pub max_size: u16,
    pub size: u16,
    pub ready: bool,
    pub msix_vector: u16,
    pub desc_table: GuestAddress,
    pub avail_ring: GuestAddress,
    pub used_ring: GuestAddress,
    next_avail: u16,
    next_used: u16,
}

impl Queue {
    pub fn new(max_size: u16) -> Self {
        Self {
            max_size,
            size: max_size,
            ready: false,
            msix_vector: NO_VECTOR,
            desc_table: GuestAddress(0),
            avail_ring: GuestAddress(0),
            used_ring: GuestAddress(0),
            next_avail: 0,
            next_used: 0,
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new(self.max_size);
    }

    pub fn is_valid(&self, mem: &GuestMemory) -> bool {
        let size = u64::from(self.size);
        let aligned = self.desc_table.raw().is_multiple_of(16)
            && self.avail_ring.raw().is_multiple_of(2)
            && self.used_ring.raw().is_multiple_of(4);
        let fits = |addr: GuestAddress, len: u64| mem.contains_range(addr, len as usize);
        self.size.is_power_of_two()
            && self.size <= self.max_size
            && aligned
            && fits(self.desc_table, DESCRIPTOR_SIZE * size)
            && fits(
                self.avail_ring,
                RING_ENTRIES_OFFSET + AVAIL_ENTRY_SIZE * size + RING_TRAILER_SIZE,
            )
            && fits(
                self.used_ring,
                RING_ENTRIES_OFFSET + USED_ENTRY_SIZE * size + RING_TRAILER_SIZE,
            )
    }

    pub fn next_avail(&self) -> u16 {
        self.next_avail
    }

    pub fn next_used(&self) -> u16 {
        self.next_used
    }

    fn avail_index(&self, mem: &GuestMemory) -> Result<u16> {
        Ok(mem.read_obj(GuestAddress(self.avail_ring.raw() + RING_INDEX_OFFSET))?)
    }

    pub fn has_available(&self, mem: &GuestMemory) -> Result<bool> {
        Ok(self.ready && self.avail_index(mem)? != self.next_avail)
    }

    pub fn pop(&mut self, mem: &GuestMemory) -> Result<Option<DescriptorChain>> {
        if !self.ready {
            return Ok(None);
        }
        let avail_index = self.avail_index(mem)?;
        fence(Ordering::Acquire);
        let pending = avail_index.wrapping_sub(self.next_avail);
        if pending == 0 {
            return Ok(None);
        }
        if pending > self.size {
            return Err(QueueError::AvailIndexAhead(pending));
        }
        let slot = u64::from(self.next_avail % self.size);
        let head: u16 = mem.read_obj(GuestAddress(
            self.avail_ring.raw() + RING_ENTRIES_OFFSET + slot * AVAIL_ENTRY_SIZE,
        ))?;
        self.next_avail = self.next_avail.wrapping_add(1);
        DescriptorChain::parse(mem, self.desc_table, self.size, head).map(Some)
    }

    pub fn undo_pop(&mut self) {
        self.next_avail = self.next_avail.wrapping_sub(1);
    }

    pub fn add_used(&mut self, mem: &GuestMemory, head: u16, len: u32) -> Result<()> {
        if !self.ready || head >= self.size {
            return Err(QueueError::NotReady);
        }
        let slot = u64::from(self.next_used % self.size);
        let element = UsedElement {
            id: u32::from(head),
            len,
        };
        mem.write_obj(
            GuestAddress(self.used_ring.raw() + RING_ENTRIES_OFFSET + slot * USED_ENTRY_SIZE),
            &element,
        )?;
        self.next_used = self.next_used.wrapping_add(1);
        fence(Ordering::Release);
        mem.write_obj(
            GuestAddress(self.used_ring.raw() + RING_INDEX_OFFSET),
            &self.next_used,
        )?;
        Ok(())
    }

    pub fn needs_notification(&self, mem: &GuestMemory) -> bool {
        fence(Ordering::SeqCst);
        mem.read_obj::<u16>(self.avail_ring)
            .map(|flags| flags & VIRTQ_AVAIL_F_NO_INTERRUPT == 0)
            .unwrap_or(true)
    }
}

#[cfg(test)]
pub(crate) mod test_utils {
    use super::*;

    pub const DESC_TABLE: u64 = 0x1000;
    pub const AVAIL_RING: u64 = 0x2000;
    pub const USED_RING: u64 = 0x3000;
    pub const DATA_AREA: u64 = 0x10000;

    pub fn memory() -> GuestMemory {
        GuestMemory::new(&[(GuestAddress(0), 0x100000)]).unwrap()
    }

    pub fn ready_queue(size: u16) -> Queue {
        let mut queue = Queue::new(size);
        queue.desc_table = GuestAddress(DESC_TABLE);
        queue.avail_ring = GuestAddress(AVAIL_RING);
        queue.used_ring = GuestAddress(USED_RING);
        queue.ready = true;
        queue
    }

    pub fn write_descriptor(
        mem: &GuestMemory,
        index: u16,
        addr: u64,
        len: u32,
        flags: u16,
        next: u16,
    ) {
        let raw = RawDescriptor {
            addr,
            len,
            flags,
            next,
        };
        mem.write_obj(GuestAddress(DESC_TABLE + u64::from(index) * 16), &raw)
            .unwrap();
    }

    pub fn publish(mem: &GuestMemory, slot: u16, head: u16, avail_index: u16) {
        mem.write_obj(GuestAddress(AVAIL_RING + 4 + u64::from(slot) * 2), &head)
            .unwrap();
        mem.write_obj(GuestAddress(AVAIL_RING + 2), &avail_index)
            .unwrap();
    }

    pub fn used(mem: &GuestMemory, slot: u16) -> (u32, u32) {
        let element: UsedElement = mem
            .read_obj(GuestAddress(USED_RING + 4 + u64::from(slot) * 8))
            .unwrap();
        (element.id, element.len)
    }

    pub fn used_index(mem: &GuestMemory) -> u16 {
        mem.read_obj(GuestAddress(USED_RING + 2)).unwrap()
    }

    pub fn chain(mem: &GuestMemory, head: u16, buffers: &[(u64, u32, bool)]) {
        for (i, (addr, len, writable)) in buffers.iter().enumerate() {
            let index = head + i as u16;
            let mut flags = if *writable { VIRTQ_DESC_F_WRITE } else { 0 };
            if i + 1 < buffers.len() {
                flags |= VIRTQ_DESC_F_NEXT;
            }
            write_descriptor(mem, index, *addr, *len, flags, index + 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::test_utils::*;
    use super::*;

    #[test]
    fn pops_chain_and_adds_used() {
        let mem = memory();
        let mut queue = ready_queue(16);
        assert!(queue.is_valid(&mem));
        chain(
            &mem,
            0,
            &[(DATA_AREA, 4, false), (DATA_AREA + 0x100, 8, true)],
        );
        publish(&mem, 0, 0, 1);
        let popped = queue.pop(&mem).unwrap().unwrap();
        assert_eq!(popped.head, 0);
        assert_eq!(popped.descriptors().len(), 2);
        assert!(popped.descriptors()[1].writable);
        assert!(queue.pop(&mem).unwrap().is_none());
        queue.add_used(&mem, popped.head, 8).unwrap();
        assert_eq!(used(&mem, 0), (0, 8));
        assert_eq!(used_index(&mem), 1);
    }

    #[test]
    fn cursor_spans_descriptors() {
        let mem = memory();
        let mut queue = ready_queue(8);
        mem.write(GuestAddress(DATA_AREA), b"hello").unwrap();
        mem.write(GuestAddress(DATA_AREA + 0x100), b"world")
            .unwrap();
        chain(
            &mem,
            2,
            &[
                (DATA_AREA, 5, false),
                (DATA_AREA + 0x100, 5, false),
                (DATA_AREA + 0x200, 3, true),
                (DATA_AREA + 0x300, 3, true),
            ],
        );
        publish(&mem, 0, 2, 1);
        let chain = queue.pop(&mem).unwrap().unwrap();
        let mut reader = chain.reader();
        assert_eq!(reader.remaining(), 10);
        let mut buf = [0u8; 7];
        assert_eq!(reader.read(&mem, &mut buf).unwrap(), 7);
        assert_eq!(&buf, b"hellowo");
        assert_eq!(reader.remaining(), 3);
        let mut writer = chain.writer();
        writer.skip(2);
        assert_eq!(writer.write(&mem, b"abcdef").unwrap(), 4);
        let mut out = [0u8; 4];
        mem.read(GuestAddress(DATA_AREA + 0x202), &mut out[..1])
            .unwrap();
        mem.read(GuestAddress(DATA_AREA + 0x300), &mut out[1..])
            .unwrap();
        assert_eq!(&out, b"abcd");
        assert_eq!(writer.remaining(), 0);
    }

    #[test]
    fn rejects_malformed_chains() {
        let mem = memory();
        let mut queue = ready_queue(4);
        write_descriptor(&mem, 0, DATA_AREA, 4, VIRTQ_DESC_F_NEXT, 0);
        publish(&mem, 0, 0, 1);
        assert!(matches!(queue.pop(&mem), Err(QueueError::ChainTooLong)));

        let mut queue = ready_queue(4);
        write_descriptor(&mem, 0, DATA_AREA, 4, VIRTQ_DESC_F_NEXT, 9);
        assert!(matches!(
            queue.pop(&mem),
            Err(QueueError::DescriptorOutOfRange(9))
        ));

        let mut queue = ready_queue(4);
        publish(&mem, 0, 7, 1);
        assert!(matches!(
            queue.pop(&mem),
            Err(QueueError::DescriptorOutOfRange(7))
        ));

        let mut queue = ready_queue(4);
        write_descriptor(&mem, 0, 0xffff_0000, 0x1000, 0, 0);
        publish(&mem, 0, 0, 1);
        assert!(matches!(
            queue.pop(&mem),
            Err(QueueError::InvalidBuffer { .. })
        ));

        let mut queue = ready_queue(4);
        write_descriptor(&mem, 0, DATA_AREA, 16, VIRTQ_DESC_F_INDIRECT, 0);
        assert!(matches!(
            queue.pop(&mem),
            Err(QueueError::IndirectDescriptor)
        ));

        let mut queue = ready_queue(4);
        publish(&mem, 0, 0, 9);
        assert!(matches!(
            queue.pop(&mem),
            Err(QueueError::AvailIndexAhead(9))
        ));
    }

    #[test]
    fn validates_ring_layout() {
        let mem = memory();
        let mut queue = ready_queue(16);
        queue.size = 12;
        assert!(!queue.is_valid(&mem));
        queue.size = 32;
        assert!(!queue.is_valid(&mem));
        let mut queue = ready_queue(16);
        queue.desc_table = GuestAddress(DESC_TABLE + 8);
        assert!(!queue.is_valid(&mem));
        let mut queue = ready_queue(16);
        queue.used_ring = GuestAddress(0xffff0);
        assert!(!queue.is_valid(&mem));
    }

    #[test]
    fn index_wraps_around() {
        let mem = memory();
        let mut queue = ready_queue(4);
        queue.next_avail = u16::MAX;
        queue.next_used = u16::MAX;
        chain(&mem, 0, &[(DATA_AREA, 1, true)]);
        publish(&mem, 3, 0, 0);
        let popped = queue.pop(&mem).unwrap().unwrap();
        assert_eq!(queue.next_avail(), 0);
        queue.add_used(&mem, popped.head, 1).unwrap();
        assert_eq!(used_index(&mem), 0);
        assert_eq!(used(&mem, 3), (0, 1));
    }

    #[test]
    fn notification_suppression() {
        let mem = memory();
        let queue = ready_queue(4);
        assert!(queue.needs_notification(&mem));
        mem.write_obj(GuestAddress(AVAIL_RING), &1u16).unwrap();
        assert!(!queue.needs_notification(&mem));
    }

    #[test]
    fn inactive_queue_yields_nothing() {
        let mem = memory();
        let mut queue = Queue::new(4);
        assert!(queue.pop(&mem).unwrap().is_none());
        assert!(queue.add_used(&mem, 0, 0).is_err());
    }

    proptest! {
        #[test]
        fn arbitrary_rings_never_panic(
            descriptors in proptest::collection::vec(any::<(u64, u32, u16, u16)>(), 8),
            heads in proptest::collection::vec(any::<u16>(), 8),
            avail_index in any::<u16>(),
            next_avail in any::<u16>(),
        ) {
            let mem = memory();
            let mut queue = ready_queue(8);
            queue.next_avail = next_avail;
            for (i, (addr, len, flags, next)) in descriptors.iter().enumerate() {
                write_descriptor(&mem, i as u16, *addr, *len, *flags, *next);
            }
            for (slot, head) in heads.iter().enumerate() {
                publish(&mem, slot as u16, *head, avail_index);
            }
            for _ in 0..16 {
                match queue.pop(&mem) {
                    Ok(Some(chain)) => {
                        let mut buf = vec![0u8; 64];
                        let _ = chain.reader().read(&mem, &mut buf);
                        let _ = chain.writer().write(&mem, &buf);
                        let _ = queue.add_used(&mem, chain.head, 1);
                    }
                    Ok(None) | Err(_) => break,
                }
            }
        }
    }
}
