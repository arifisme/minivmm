use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::devices::acpi_pm::AcpiPmState;
use crate::devices::i8042::I8042State;
use crate::devices::pci::root::PciRootState;
use crate::devices::serial::SerialState;
use crate::devices::virtio::pci::VirtioPciState;
use crate::kvm::irq::RoutingState;
use crate::kvm::vcpu::VcpuState;
use crate::kvm::vm::VmState;
use crate::memory::{GuestMemory, MemoryError};
use crate::vmm::MachineConfig;

const MAGIC: &[u8; 8] = b"MVMSNAP\0";
pub const FORMAT_VERSION: u32 = 1;
const MAX_STATE_LEN: u64 = 64 << 20;

#[derive(Debug, Error)]
pub enum SnapshotError {
    #[error("not a minivmm snapshot")]
    BadMagic,
    #[error("snapshot format version {0} is not supported (expected {FORMAT_VERSION})")]
    UnsupportedVersion(u32),
    #[error("snapshot state is corrupt: {0}")]
    Corrupt(String),
    #[error("snapshot I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Memory(#[from] MemoryError),
}

pub type Result<T> = std::result::Result<T, SnapshotError>;

#[derive(Serialize, Deserialize)]
pub struct VmSnapshot {
    pub config: MachineConfig,
    pub vm: VmState,
    pub routing: RoutingState,
    pub vcpus: Vec<VcpuState>,
    pub serial: SerialState,
    pub i8042: I8042State,
    pub acpi_pm: AcpiPmState,
    pub pci: PciRootState,
    pub block: Option<VirtioPciState<()>>,
    pub net: Option<VirtioPciState<()>>,
}

pub fn write_snapshot(path: &Path, snapshot: &VmSnapshot, memory: &GuestMemory) -> Result<()> {
    let state =
        postcard::to_allocvec(snapshot).map_err(|e| SnapshotError::Corrupt(e.to_string()))?;
    let partial = path.with_extension("partial");
    {
        let mut out = BufWriter::new(File::create(&partial)?);
        out.write_all(MAGIC)?;
        out.write_all(&FORMAT_VERSION.to_le_bytes())?;
        out.write_all(&(state.len() as u64).to_le_bytes())?;
        out.write_all(&state)?;
        for region in memory.regions() {
            memory.write_to(region.start(), &mut out, region.len())?;
        }
        out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
    }
    fs::rename(&partial, path)?;
    Ok(())
}

pub struct SnapshotReader {
    reader: BufReader<File>,
}

impl SnapshotReader {
    pub fn open(path: &Path) -> Result<(VmSnapshot, Self)> {
        let mut reader = BufReader::new(File::open(path)?);
        let mut magic = [0u8; 8];
        reader.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(SnapshotError::BadMagic);
        }
        let mut word = [0u8; 4];
        reader.read_exact(&mut word)?;
        let version = u32::from_le_bytes(word);
        if version != FORMAT_VERSION {
            return Err(SnapshotError::UnsupportedVersion(version));
        }
        let mut len = [0u8; 8];
        reader.read_exact(&mut len)?;
        let len = u64::from_le_bytes(len);
        if len > MAX_STATE_LEN {
            return Err(SnapshotError::Corrupt(format!(
                "state length {len} is too large"
            )));
        }
        let mut state = vec![0u8; len as usize];
        reader.read_exact(&mut state)?;
        let snapshot =
            postcard::from_bytes(&state).map_err(|e| SnapshotError::Corrupt(e.to_string()))?;
        Ok((snapshot, Self { reader }))
    }

    pub fn load_memory(mut self, memory: &GuestMemory) -> Result<()> {
        for region in memory.regions() {
            memory.read_from(region.start(), &mut self.reader, region.len())?;
        }
        let mut extra = [0u8; 1];
        if self.reader.read(&mut extra)? != 0 {
            return Err(SnapshotError::Corrupt(
                "trailing data after guest memory".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use vmm_sys_util::tempdir::TempDir;

    use super::*;
    use crate::kvm::vcpu::VcpuState;
    use crate::memory::GuestAddress;
    use crate::vmm::{DiskConfig, MachineConfig};

    const MEMORY_SIZE: usize = 0x2000;

    fn memory() -> GuestMemory {
        GuestMemory::new(&[(GuestAddress(0), MEMORY_SIZE)]).unwrap()
    }

    fn snapshot_bytes(dir: &TempDir) -> Vec<u8> {
        let snapshot = VmSnapshot {
            config: MachineConfig {
                cpus: 1,
                mem_size: MEMORY_SIZE as u64,
                disk: Some(DiskConfig {
                    path: "/nonexistent/disk.img".into(),
                    read_only: true,
                }),
                net: None,
            },
            vm: VmState::default(),
            routing: RoutingState::default(),
            vcpus: vec![VcpuState::default()],
            serial: SerialState::default(),
            i8042: I8042State::default(),
            acpi_pm: AcpiPmState::default(),
            pci: PciRootState::default(),
            block: None,
            net: None,
        };
        let memory = memory();
        let pattern: Vec<u8> = (0..MEMORY_SIZE).map(|i| (i * 7) as u8).collect();
        memory.write(GuestAddress(0), &pattern).unwrap();
        let path = dir.as_path().join("vm.snap");
        write_snapshot(&path, &snapshot, &memory).unwrap();
        fs::read(path).unwrap()
    }

    fn restore(dir: &TempDir, bytes: &[u8]) -> Result<GuestMemory> {
        let path = dir.as_path().join("input.snap");
        fs::write(&path, bytes).unwrap();
        let (_, reader) = SnapshotReader::open(&path)?;
        let memory = memory();
        reader.load_memory(&memory)?;
        Ok(memory)
    }

    fn state_end(bytes: &[u8]) -> usize {
        20 + u64::from_le_bytes(bytes[12..20].try_into().unwrap()) as usize
    }

    #[test]
    fn round_trip_restores_memory() {
        let dir = TempDir::new().unwrap();
        let bytes = snapshot_bytes(&dir);
        let restored = restore(&dir, &bytes).unwrap();
        let mut contents = vec![0u8; MEMORY_SIZE];
        restored.read(GuestAddress(0), &mut contents).unwrap();
        assert_eq!(&bytes[bytes.len() - MEMORY_SIZE..], &contents[..]);
    }

    #[test]
    fn every_truncation_is_an_error() {
        let dir = TempDir::new().unwrap();
        let bytes = snapshot_bytes(&dir);
        let cuts = (0..state_end(&bytes) + 64).chain((state_end(&bytes)..bytes.len()).step_by(509));
        for len in cuts {
            assert!(restore(&dir, &bytes[..len]).is_err(), "truncated to {len}");
        }
    }

    #[test]
    fn corrupted_headers_are_rejected() {
        let dir = TempDir::new().unwrap();
        let bytes = snapshot_bytes(&dir);
        let mut magic = bytes.clone();
        magic[0] ^= 1;
        assert!(matches!(
            restore(&dir, &magic),
            Err(SnapshotError::BadMagic)
        ));
        let mut version = bytes.clone();
        version[8] = 2;
        assert!(matches!(
            restore(&dir, &version),
            Err(SnapshotError::UnsupportedVersion(2))
        ));
        let mut length = bytes.clone();
        length[12..20].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(matches!(
            restore(&dir, &length),
            Err(SnapshotError::Corrupt(_))
        ));
        let mut trailing = bytes;
        trailing.push(0);
        assert!(matches!(
            restore(&dir, &trailing),
            Err(SnapshotError::Corrupt(_))
        ));
    }

    #[test]
    fn corrupted_state_never_panics() {
        let dir = TempDir::new().unwrap();
        let bytes = snapshot_bytes(&dir);
        for index in 20..state_end(&bytes) {
            for flip in [0x01, 0x80, 0xff] {
                let mut corrupted = bytes.clone();
                corrupted[index] ^= flip;
                let _ = restore(&dir, &corrupted);
            }
        }
    }
}
