use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock};

use thiserror::Error;

use crate::sync::{LockExt, RwLockExt};

pub trait BusDevice: Send {
    fn read(&mut self, offset: u64, data: &mut [u8]);
    fn write(&mut self, offset: u64, data: &[u8]);
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum BusError {
    #[error("bus range {base:#x}+{len:#x} overlaps an existing device or is invalid")]
    Overlap { base: u64, len: u64 },
    #[error("no device registered at {0:#x}")]
    NotFound(u64),
}

#[derive(Clone)]
struct Entry {
    len: u64,
    device: Arc<Mutex<dyn BusDevice>>,
}

#[derive(Default)]
pub struct Bus {
    entries: RwLock<BTreeMap<u64, Entry>>,
}

fn overlaps(map: &BTreeMap<u64, Entry>, base: u64, len: u64) -> bool {
    let Some(end) = base.checked_add(len) else {
        return true;
    };
    if len == 0 {
        return true;
    }
    let before = map
        .range(..=base)
        .next_back()
        .is_some_and(|(start, entry)| start + entry.len > base);
    let after = map.range(base..end).next().is_some();
    before || after
}

impl Bus {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(
        &self,
        device: Arc<Mutex<dyn BusDevice>>,
        base: u64,
        len: u64,
    ) -> Result<(), BusError> {
        let mut map = self.entries.write_or_recover();
        if overlaps(&map, base, len) {
            return Err(BusError::Overlap { base, len });
        }
        map.insert(base, Entry { len, device });
        Ok(())
    }

    pub fn relocate(&self, old_base: u64, new_base: u64) -> Result<(), BusError> {
        let mut map = self.entries.write_or_recover();
        let entry = map.remove(&old_base).ok_or(BusError::NotFound(old_base))?;
        if overlaps(&map, new_base, entry.len) {
            let len = entry.len;
            map.insert(old_base, entry);
            return Err(BusError::Overlap {
                base: new_base,
                len,
            });
        }
        map.insert(new_base, entry);
        Ok(())
    }

    fn resolve(&self, addr: u64, len: usize) -> Option<(u64, Arc<Mutex<dyn BusDevice>>)> {
        if len == 0 {
            return None;
        }
        let map = self.entries.read_or_recover();
        let (base, entry) = map.range(..=addr).next_back()?;
        let offset = addr - base;
        let end = offset.checked_add(len as u64)?;
        (end <= entry.len).then(|| (offset, entry.device.clone()))
    }

    pub fn read(&self, addr: u64, data: &mut [u8]) -> bool {
        match self.resolve(addr, data.len()) {
            Some((offset, device)) => {
                device.lock_or_recover().read(offset, data);
                true
            }
            None => false,
        }
    }

    pub fn write(&self, addr: u64, data: &[u8]) -> bool {
        match self.resolve(addr, data.len()) {
            Some((offset, device)) => {
                device.lock_or_recover().write(offset, data);
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Recorder {
        last_write: Option<(u64, Vec<u8>)>,
    }

    impl BusDevice for Recorder {
        fn read(&mut self, offset: u64, data: &mut [u8]) {
            data.fill(offset as u8);
        }

        fn write(&mut self, offset: u64, data: &[u8]) {
            self.last_write = Some((offset, data.to_vec()));
        }
    }

    #[test]
    fn dispatches_by_range() {
        let bus = Bus::new();
        let dev = Arc::new(Mutex::new(Recorder::default()));
        bus.insert(dev.clone(), 0x100, 0x10).unwrap();
        let mut data = [0u8; 2];
        assert!(bus.read(0x104, &mut data));
        assert_eq!(data, [4, 4]);
        assert!(bus.write(0x10e, &[1, 2]));
        assert_eq!(dev.lock().unwrap().last_write, Some((0xe, vec![1, 2])));
        assert!(!bus.read(0x10f, &mut data));
        assert!(!bus.read(0xff, &mut [0u8; 1]));
        assert!(!bus.write(0x110, &[0]));
        assert!(!bus.read(0x104, &mut []));
    }

    #[test]
    fn rejects_overlaps() {
        let bus = Bus::new();
        let dev = Arc::new(Mutex::new(Recorder::default()));
        bus.insert(dev.clone(), 0x100, 0x10).unwrap();
        assert!(bus.insert(dev.clone(), 0x10f, 1).is_err());
        assert!(bus.insert(dev.clone(), 0xf8, 0x10).is_err());
        assert!(bus.insert(dev.clone(), 0x0, 0).is_err());
        assert!(bus.insert(dev.clone(), u64::MAX, 2).is_err());
        assert!(bus.insert(dev.clone(), 0x110, 0x10).is_ok());
        assert!(bus.insert(dev, 0xf0, 0x10).is_ok());
    }

    #[test]
    fn relocates_devices() {
        let bus = Bus::new();
        let dev = Arc::new(Mutex::new(Recorder::default()));
        bus.insert(dev.clone(), 0x100, 0x10).unwrap();
        bus.insert(dev, 0x200, 0x10).unwrap();
        assert!(bus.relocate(0x100, 0x1f8).is_err());
        assert!(bus.read(0x100, &mut [0u8; 1]));
        bus.relocate(0x100, 0x300).unwrap();
        assert!(!bus.read(0x100, &mut [0u8; 1]));
        assert!(bus.read(0x30f, &mut [0u8; 1]));
        assert_eq!(bus.relocate(0x999, 0), Err(BusError::NotFound(0x999)));
    }
}
