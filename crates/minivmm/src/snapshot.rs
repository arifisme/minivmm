use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

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
use crate::memory::{GuestAddress, GuestMemory, MemoryError};
use crate::vmm::MachineConfig;

const MAGIC: &[u8; 8] = b"MVMSNAP\0";
pub const FORMAT_VERSION: u32 = 2;
const HEADER_LEN: usize = 20;
const MAX_STATE_LEN: u64 = 64 << 20;
const MEMORY_SUFFIX: &str = ".mem";

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

pub fn default_memory_path(state_path: &Path) -> PathBuf {
    let mut path = state_path.as_os_str().to_owned();
    path.push(MEMORY_SUFFIX);
    PathBuf::from(path)
}

fn partial_path(path: &Path) -> PathBuf {
    let mut partial = path.as_os_str().to_owned();
    partial.push(".partial");
    PathBuf::from(partial)
}

pub fn write_snapshot(
    state_path: &Path,
    memory_path: &Path,
    snapshot: &VmSnapshot,
    memory: &GuestMemory,
) -> Result<()> {
    let state =
        postcard::to_allocvec(snapshot).map_err(|e| SnapshotError::Corrupt(e.to_string()))?;
    let partial_memory = partial_path(memory_path);
    let file = File::create(&partial_memory)?;
    memory.dump_sparse(&file)?;
    file.sync_all()?;
    fs::rename(&partial_memory, memory_path)?;

    let partial_state = partial_path(state_path);
    let mut out = File::create(&partial_state)?;
    let mut header = Vec::with_capacity(HEADER_LEN + state.len());
    header.extend_from_slice(MAGIC);
    header.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    header.extend_from_slice(&(state.len() as u64).to_le_bytes());
    header.extend_from_slice(&state);
    out.write_all(&header)?;
    out.sync_all()?;
    fs::rename(&partial_state, state_path)?;
    Ok(())
}

pub fn read_snapshot(path: &Path) -> Result<VmSnapshot> {
    let mut file = File::open(path)?;
    let mut header = [0u8; HEADER_LEN];
    file.read_exact(&mut header)?;
    if &header[..8] != MAGIC {
        return Err(SnapshotError::BadMagic);
    }
    let version = u32::from_le_bytes([header[8], header[9], header[10], header[11]]);
    if version != FORMAT_VERSION {
        return Err(SnapshotError::UnsupportedVersion(version));
    }
    let mut len = [0u8; 8];
    len.copy_from_slice(&header[12..HEADER_LEN]);
    let len = u64::from_le_bytes(len);
    if len > MAX_STATE_LEN {
        return Err(SnapshotError::Corrupt(format!(
            "state length {len} is too large"
        )));
    }
    let mut state = Vec::new();
    file.take(len + 1).read_to_end(&mut state)?;
    if state.len() as u64 != len {
        return Err(SnapshotError::Corrupt(format!(
            "state holds {} bytes instead of {len}",
            state.len()
        )));
    }
    postcard::from_bytes(&state).map_err(|e| SnapshotError::Corrupt(e.to_string()))
}

pub fn map_memory(path: &Path, layout: &[(GuestAddress, usize)]) -> Result<GuestMemory> {
    let file = File::open(path)?;
    Ok(GuestMemory::from_file(layout, &file)?)
}

#[cfg(test)]
mod tests {
    use vmm_sys_util::tempdir::TempDir;

    use super::*;
    use crate::kvm::vcpu::VcpuState;
    use crate::vmm::{DiskConfig, MachineConfig};

    const MEMORY_SIZE: usize = 0x2000;
    const LAYOUT: [(GuestAddress, usize); 1] = [(GuestAddress(0), MEMORY_SIZE)];

    fn snapshot_files(dir: &TempDir) -> (Vec<u8>, PathBuf) {
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
        let memory = GuestMemory::new(&LAYOUT).unwrap();
        let pattern: Vec<u8> = (0..MEMORY_SIZE).map(|i| (i * 7) as u8).collect();
        memory.write(GuestAddress(0), &pattern).unwrap();
        let path = dir.as_path().join("vm.snap");
        write_snapshot(&path, &default_memory_path(&path), &snapshot, &memory).unwrap();
        (fs::read(&path).unwrap(), default_memory_path(&path))
    }

    fn decode(dir: &TempDir, bytes: &[u8]) -> Result<VmSnapshot> {
        let path = dir.as_path().join("input.snap");
        fs::write(&path, bytes).unwrap();
        read_snapshot(&path)
    }

    #[test]
    fn round_trip_maps_memory() {
        let dir = TempDir::new().unwrap();
        let (bytes, memory_path) = snapshot_files(&dir);
        let snapshot = decode(&dir, &bytes).unwrap();
        assert_eq!(snapshot.config.mem_size, MEMORY_SIZE as u64);
        assert_eq!(memory_path, dir.as_path().join("vm.snap.mem"));
        let restored = map_memory(&memory_path, &LAYOUT).unwrap();
        let mut contents = vec![0u8; MEMORY_SIZE];
        restored.read(GuestAddress(0), &mut contents).unwrap();
        assert_eq!(contents, fs::read(&memory_path).unwrap());
        assert_eq!(contents[7], 49);
        assert!(map_memory(&memory_path, &[(GuestAddress(0), MEMORY_SIZE * 2)]).is_err());
    }

    #[test]
    fn every_truncation_is_an_error() {
        let dir = TempDir::new().unwrap();
        let (bytes, _) = snapshot_files(&dir);
        for len in 0..bytes.len() {
            assert!(decode(&dir, &bytes[..len]).is_err(), "truncated to {len}");
        }
    }

    #[test]
    fn corrupted_headers_are_rejected() {
        let dir = TempDir::new().unwrap();
        let (bytes, _) = snapshot_files(&dir);
        let mut magic = bytes.clone();
        magic[0] ^= 1;
        assert!(matches!(decode(&dir, &magic), Err(SnapshotError::BadMagic)));
        let mut version = bytes.clone();
        version[8] = 1;
        assert!(matches!(
            decode(&dir, &version),
            Err(SnapshotError::UnsupportedVersion(1))
        ));
        let mut length = bytes.clone();
        length[12..20].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(matches!(
            decode(&dir, &length),
            Err(SnapshotError::Corrupt(_))
        ));
        let mut trailing = bytes;
        trailing.push(0);
        assert!(matches!(
            decode(&dir, &trailing),
            Err(SnapshotError::Corrupt(_))
        ));
    }

    #[test]
    fn corrupted_state_never_panics() {
        let dir = TempDir::new().unwrap();
        let (bytes, _) = snapshot_files(&dir);
        for index in HEADER_LEN..bytes.len() {
            for flip in [0x01, 0x80, 0xff] {
                let mut corrupted = bytes.clone();
                corrupted[index] ^= flip;
                let _ = decode(&dir, &corrupted);
            }
        }
    }
}
