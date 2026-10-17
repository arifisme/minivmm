use std::fs;
use std::sync::{Arc, Mutex};

use mv_proto::api::CapacityInfo;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Denied {
    Memory,
    HostMemory,
    Cpu,
}

impl Denied {
    pub fn reason(self) -> &'static str {
        match self {
            Denied::Memory => "memory",
            Denied::HostMemory => "host_memory",
            Denied::Cpu => "cpu",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub memory_total_mib: u64,
    pub memory_limit_mib: u64,
    pub memory_reserve_mib: u64,
    pub cpus: u32,
    pub vcpu_limit: u32,
}

#[derive(Default)]
struct Usage {
    memory_mib: u64,
    vcpus: u32,
}

type MemoryProbe = Box<dyn Fn() -> Option<u64> + Send + Sync>;

/// Tracks the memory promised to live VMs and the vCPUs busy with invocations, and refuses
/// work that would exceed either limit or leave the host short of free memory.
pub struct Admission {
    limits: Limits,
    usage: Mutex<Usage>,
    available_mib: MemoryProbe,
}

#[must_use]
pub struct MemoryReservation {
    admission: Arc<Admission>,
    mib: u64,
}

#[must_use]
pub struct CpuReservation {
    admission: Arc<Admission>,
    vcpus: u32,
}

impl Admission {
    pub fn new(limits: Limits, available_mib: MemoryProbe) -> Arc<Self> {
        Arc::new(Self {
            limits,
            usage: Mutex::new(Usage::default()),
            available_mib,
        })
    }

    fn usage(&self) -> std::sync::MutexGuard<'_, Usage> {
        self.usage.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn reserve_memory(self: &Arc<Self>, mib: u64) -> Result<MemoryReservation, Denied> {
        let mut usage = self.usage();
        if usage.memory_mib + mib > self.limits.memory_limit_mib {
            return Err(Denied::Memory);
        }
        if (self.available_mib)().is_some_and(|free| free < self.limits.memory_reserve_mib) {
            return Err(Denied::HostMemory);
        }
        usage.memory_mib += mib;
        Ok(MemoryReservation {
            admission: self.clone(),
            mib,
        })
    }

    pub fn reserve_cpu(self: &Arc<Self>, vcpus: u32) -> Result<CpuReservation, Denied> {
        let mut usage = self.usage();
        if usage.vcpus + vcpus > self.limits.vcpu_limit {
            return Err(Denied::Cpu);
        }
        usage.vcpus += vcpus;
        Ok(CpuReservation {
            admission: self.clone(),
            vcpus,
        })
    }

    pub fn capacity(&self) -> CapacityInfo {
        let usage = self.usage();
        CapacityInfo {
            memory_total_mib: self.limits.memory_total_mib,
            memory_available_mib: (self.available_mib)().unwrap_or(0),
            memory_reserved_mib: usage.memory_mib,
            memory_limit_mib: self.limits.memory_limit_mib,
            cpus: self.limits.cpus,
            vcpus_busy: usage.vcpus,
            vcpu_limit: self.limits.vcpu_limit,
        }
    }
}

impl Drop for MemoryReservation {
    fn drop(&mut self) {
        let mut usage = self.admission.usage();
        usage.memory_mib = usage.memory_mib.saturating_sub(self.mib);
    }
}

impl Drop for CpuReservation {
    fn drop(&mut self) {
        let mut usage = self.admission.usage();
        usage.vcpus = usage.vcpus.saturating_sub(self.vcpus);
    }
}

pub fn meminfo_mib(field: &str) -> Option<u64> {
    let meminfo = fs::read_to_string("/proc/meminfo").ok()?;
    meminfo
        .lines()
        .find_map(|line| line.strip_prefix(field)?.strip_prefix(':'))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|kib| kib.parse::<u64>().ok())
        .map(|kib| kib / 1024)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    fn admission(free: Arc<AtomicU64>) -> Arc<Admission> {
        Admission::new(
            Limits {
                memory_total_mib: 4096,
                memory_limit_mib: 1000,
                memory_reserve_mib: 256,
                cpus: 2,
                vcpu_limit: 3,
            },
            Box::new(move || Some(free.load(Ordering::SeqCst))),
        )
    }

    #[test]
    fn memory_reservations_are_bounded_and_released_on_drop() {
        let free = Arc::new(AtomicU64::new(2048));
        let admission = admission(free.clone());
        let a = admission.reserve_memory(600).unwrap();
        assert_eq!(admission.reserve_memory(500).err(), Some(Denied::Memory));
        let b = admission.reserve_memory(400).unwrap();
        assert_eq!(admission.capacity().memory_reserved_mib, 1000);
        drop(a);
        free.store(100, Ordering::SeqCst);
        assert_eq!(admission.reserve_memory(10).err(), Some(Denied::HostMemory));
        free.store(2048, Ordering::SeqCst);
        let c = admission.reserve_memory(600).unwrap();
        drop((b, c));
        assert_eq!(admission.capacity().memory_reserved_mib, 0);
    }

    #[test]
    fn cpu_reservations_cap_busy_vcpus() {
        let admission = admission(Arc::new(AtomicU64::new(4096)));
        let two = admission.reserve_cpu(2).unwrap();
        let one = admission.reserve_cpu(1).unwrap();
        assert_eq!(admission.reserve_cpu(1).err(), Some(Denied::Cpu));
        assert_eq!(admission.capacity().vcpus_busy, 3);
        drop(two);
        assert!(admission.reserve_cpu(2).is_ok());
        drop(one);
        assert_eq!(Denied::HostMemory.reason(), "host_memory");
    }

    #[test]
    fn reads_meminfo() {
        assert!(meminfo_mib("MemTotal").unwrap() > 0);
        assert_eq!(meminfo_mib("NoSuchField"), None);
    }
}
