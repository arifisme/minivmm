use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

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
