use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};

use kvm_bindings::CpuId;
use kvm_bindings::{
    KVM_MAX_CPUID_ENTRIES, KVM_MAX_MSR_ENTRIES, KVM_SYSTEM_EVENT_RESET, KVM_SYSTEM_EVENT_SHUTDOWN,
    Msrs, kvm_cpuid_entry2, kvm_lapic_state, kvm_mp_state, kvm_msr_entry, kvm_regs, kvm_sregs,
    kvm_vcpu_events, kvm_xcrs, kvm_xsave,
};
use kvm_ioctls::{Kvm, VcpuExit, VcpuFd, VmFd};
use log::{debug, error, warn};
use serde::{Deserialize, Serialize};

use super::{IoctlContext, KvmError, Result};
use crate::arch::cpuid::{CpuTopology, configure_cpuid};
use crate::arch::regs::{
    boot_fpu, boot_msrs, boot_regs, configure_lapic_lints, configure_long_mode,
};
use crate::devices::bus::Bus;
use crate::exit::{ExitReason, ExitSignal};
use crate::memory::{GuestAddress, GuestMemory};
use crate::sys::signal::KickTarget;

#[derive(Serialize, Deserialize)]
pub struct VcpuState {
    cpuid: Vec<kvm_cpuid_entry2>,
    regs: kvm_regs,
    sregs: kvm_sregs,
    xsave: kvm_xsave,
    xcrs: kvm_xcrs,
    msrs: Vec<kvm_msr_entry>,
    lapic: kvm_lapic_state,
    mp_state: kvm_mp_state,
    events: kvm_vcpu_events,
}

pub struct BootSetup<'a> {
    pub memory: &'a GuestMemory,
    pub entry: GuestAddress,
    pub zero_page: GuestAddress,
}

pub enum VcpuRequest {
    Pause,
    Resume,
    SaveState,
    Stop,
}

pub enum VcpuResponse {
    Paused,
    Resumed,
    State(Box<VcpuState>),
    Failed(String),
}

pub struct Vcpu {
    index: u8,
    fd: VcpuFd,
    pio: Arc<Bus>,
    mmio: Arc<Bus>,
    exit: Arc<ExitSignal>,
    msr_indices: Vec<u32>,
}

enum RunOutcome {
    Continue,
    Exit(ExitReason),
}

impl Vcpu {
    pub fn new(
        kvm: &Kvm,
        vm: &VmFd,
        index: u8,
        pio: Arc<Bus>,
        mmio: Arc<Bus>,
        exit: Arc<ExitSignal>,
    ) -> Result<Self> {
        let fd = vm.create_vcpu(u64::from(index)).ctx("KVM_CREATE_VCPU")?;
        let msr_indices = kvm
            .get_msr_index_list()
            .ctx("KVM_GET_MSR_INDEX_LIST")?
            .as_slice()
            .to_vec();
        Ok(Self {
            index,
            fd,
            pio,
            mmio,
            exit,
            msr_indices,
        })
    }

    pub fn index(&self) -> u8 {
        self.index
    }

    pub fn configure(&self, kvm: &Kvm, cpu_count: u8, boot: Option<&BootSetup>) -> Result<()> {
        let mut cpuid = kvm
            .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
            .ctx("KVM_GET_SUPPORTED_CPUID")?;
        configure_cpuid(
            cpuid.as_mut_slice(),
            CpuTopology {
                apic_id: self.index,
                cpu_count,
            },
        );
        self.fd.set_cpuid2(&cpuid).ctx("KVM_SET_CPUID2")?;
        self.set_msrs(&boot_msrs())?;
        self.fd.set_fpu(&boot_fpu()).ctx("KVM_SET_FPU")?;
        let mut lapic = self.fd.get_lapic().ctx("KVM_GET_LAPIC")?;
        configure_lapic_lints(&mut lapic);
        self.fd.set_lapic(&lapic).ctx("KVM_SET_LAPIC")?;
        if let Some(boot) = boot {
            let mut sregs = self.fd.get_sregs().ctx("KVM_GET_SREGS")?;
            configure_long_mode(boot.memory, &mut sregs)
                .map_err(|e| KvmError::Unsupported(e.to_string()))?;
            self.fd.set_sregs(&sregs).ctx("KVM_SET_SREGS")?;
            self.fd
                .set_regs(&boot_regs(boot.entry, boot.zero_page))
                .ctx("KVM_SET_REGS")?;
        }
        Ok(())
    }

    fn set_msrs(&self, entries: &[kvm_msr_entry]) -> Result<()> {
        for chunk in entries.chunks(KVM_MAX_MSR_ENTRIES) {
            let msrs = Msrs::from_entries(chunk)
                .map_err(|_| KvmError::Unsupported("MSR list too long".into()))?;
            let written = self.fd.set_msrs(&msrs).ctx("KVM_SET_MSRS")?;
            if written != chunk.len() {
                return Err(KvmError::Unsupported(format!(
                    "KVM rejected MSR {:#x}",
                    chunk[written].index
                )));
            }
        }
        Ok(())
    }

    fn get_msrs(&self) -> Result<Vec<kvm_msr_entry>> {
        let mut saved = Vec::new();
        let mut pending: Vec<u32> = self.msr_indices.clone();
        while !pending.is_empty() {
            let batch: Vec<kvm_msr_entry> = pending
                .iter()
                .take(KVM_MAX_MSR_ENTRIES)
                .map(|&index| kvm_msr_entry {
                    index,
                    ..Default::default()
                })
                .collect();
            let mut msrs = Msrs::from_entries(&batch)
                .map_err(|_| KvmError::Unsupported("MSR list too long".into()))?;
            let read = self.fd.get_msrs(&mut msrs).ctx("KVM_GET_MSRS")?;
            saved.extend_from_slice(&msrs.as_slice()[..read]);
            let consumed = if read < batch.len() {
                debug!(
                    "vcpu {}: MSR {:#x} is not readable",
                    self.index, batch[read].index
                );
                read + 1
            } else {
                read
            };
            pending.drain(..consumed);
        }
        Ok(saved)
    }

    pub fn save_state(&self) -> Result<VcpuState> {
        let cpuid = self
            .fd
            .get_cpuid2(KVM_MAX_CPUID_ENTRIES)
            .ctx("KVM_GET_CPUID2")?;
        Ok(VcpuState {
            cpuid: cpuid.as_slice().to_vec(),
            mp_state: self.fd.get_mp_state().ctx("KVM_GET_MP_STATE")?,
            regs: self.fd.get_regs().ctx("KVM_GET_REGS")?,
            sregs: self.fd.get_sregs().ctx("KVM_GET_SREGS")?,
            xsave: self.fd.get_xsave().ctx("KVM_GET_XSAVE")?,
            xcrs: self.fd.get_xcrs().ctx("KVM_GET_XCRS")?,
            lapic: self.fd.get_lapic().ctx("KVM_GET_LAPIC")?,
            msrs: self.get_msrs()?,
            events: self.fd.get_vcpu_events().ctx("KVM_GET_VCPU_EVENTS")?,
        })
    }

    pub fn restore_state(&self, state: &VcpuState) -> Result<()> {
        let cpuid = CpuId::from_entries(&state.cpuid)
            .map_err(|_| KvmError::Unsupported("CPUID table too long".into()))?;
        self.fd.set_cpuid2(&cpuid).ctx("KVM_SET_CPUID2")?;
        self.fd
            .set_mp_state(state.mp_state)
            .ctx("KVM_SET_MP_STATE")?;
        self.fd.set_sregs(&state.sregs).ctx("KVM_SET_SREGS")?;
        self.fd.set_regs(&state.regs).ctx("KVM_SET_REGS")?;
        // SAFETY: the buffer is a legacy-sized kvm_xsave obtained from KVM_GET_XSAVE.
        unsafe { self.fd.set_xsave(&state.xsave) }.ctx("KVM_SET_XSAVE")?;
        self.fd.set_xcrs(&state.xcrs).ctx("KVM_SET_XCRS")?;
        self.fd.set_lapic(&state.lapic).ctx("KVM_SET_LAPIC")?;
        self.set_msrs(&state.msrs)?;
        self.fd
            .set_vcpu_events(&state.events)
            .ctx("KVM_SET_VCPU_EVENTS")?;
        Ok(())
    }

    fn run_once(&mut self) -> RunOutcome {
        match self.fd.run() {
            Ok(VcpuExit::IoIn(port, data)) => {
                if !self.pio.read(u64::from(port), data) {
                    data.fill(0xff);
                }
                RunOutcome::Continue
            }
            Ok(VcpuExit::IoOut(port, data)) => {
                self.pio.write(u64::from(port), data);
                RunOutcome::Continue
            }
            Ok(VcpuExit::MmioRead(addr, data)) => {
                if !self.mmio.read(addr, data) {
                    data.fill(0xff);
                }
                RunOutcome::Continue
            }
            Ok(VcpuExit::MmioWrite(addr, data)) => {
                self.mmio.write(addr, data);
                RunOutcome::Continue
            }
            Ok(VcpuExit::Hlt) | Ok(VcpuExit::Intr) => RunOutcome::Continue,
            // A triple fault resets an x86 machine.
            Ok(VcpuExit::Shutdown) => RunOutcome::Exit(ExitReason::Reboot),
            Ok(VcpuExit::SystemEvent(kind, _)) => match kind {
                KVM_SYSTEM_EVENT_SHUTDOWN => RunOutcome::Exit(ExitReason::PowerOff),
                KVM_SYSTEM_EVENT_RESET => RunOutcome::Exit(ExitReason::Reboot),
                other => {
                    error!("vcpu {}: unexpected system event {other}", self.index);
                    RunOutcome::Exit(ExitReason::Fault)
                }
            },
            Ok(VcpuExit::FailEntry(reason, cpu)) => {
                error!(
                    "vcpu {}: VM entry failed (reason {reason:#x}, host cpu {cpu})",
                    self.index
                );
                self.dump_registers();
                RunOutcome::Exit(ExitReason::Fault)
            }
            Ok(VcpuExit::InternalError) => {
                error!("vcpu {}: KVM internal error", self.index);
                self.dump_registers();
                RunOutcome::Exit(ExitReason::Fault)
            }
            Ok(other) => {
                error!("vcpu {}: unhandled exit {other:?}", self.index);
                self.dump_registers();
                RunOutcome::Exit(ExitReason::Fault)
            }
            Err(err) if err.errno() == libc::EINTR || err.errno() == libc::EAGAIN => {
                self.fd.set_kvm_immediate_exit(0);
                RunOutcome::Continue
            }
            Err(err) => {
                error!("vcpu {}: KVM_RUN failed: {err}", self.index);
                RunOutcome::Exit(ExitReason::Fault)
            }
        }
    }

    fn dump_registers(&self) {
        if let (Ok(regs), Ok(sregs)) = (self.fd.get_regs(), self.fd.get_sregs()) {
            error!(
                "vcpu {}: rip={:#x} rsp={:#x} rflags={:#x} cr0={:#x} cr3={:#x} cr4={:#x} efer={:#x}",
                self.index,
                regs.rip,
                regs.rsp,
                regs.rflags,
                sregs.cr0,
                sregs.cr3,
                sregs.cr4,
                sregs.efer
            );
        }
    }

    fn complete_pending_io(&mut self) {
        self.fd.set_kvm_immediate_exit(1);
        match self.fd.run() {
            Err(err) if err.errno() == libc::EINTR => {}
            Ok(exit) => debug!("vcpu {}: exit while draining: {exit:?}", self.index),
            Err(err) => warn!("vcpu {}: failed to complete pending I/O: {err}", self.index),
        }
        self.fd.set_kvm_immediate_exit(0);
    }

    fn kick_target(&mut self) -> KickTarget {
        KickTarget::new(self.fd.get_kvm_run())
    }

    pub fn run(
        mut self,
        requests: Receiver<(u64, VcpuRequest)>,
        responses: Sender<(u8, u64, VcpuResponse)>,
    ) {
        // Locals drop before `self`, so even when unwinding the kick handler is detached
        // before the kvm_run mapping goes away.
        let kick_target = self.kick_target();
        kick_target.install();
        let mut paused = false;
        loop {
            let request = if paused {
                match requests.recv() {
                    Ok(request) => Some(request),
                    Err(_) => break,
                }
            } else {
                match requests.try_recv() {
                    Ok(request) => Some(request),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => break,
                }
            };
            if let Some((sequence, request)) = request {
                let response = match request {
                    VcpuRequest::Pause => {
                        if !paused {
                            self.complete_pending_io();
                        }
                        paused = true;
                        VcpuResponse::Paused
                    }
                    VcpuRequest::Resume => {
                        paused = false;
                        VcpuResponse::Resumed
                    }
                    VcpuRequest::SaveState => match self.save_state() {
                        Ok(state) => VcpuResponse::State(Box::new(state)),
                        Err(err) => VcpuResponse::Failed(err.to_string()),
                    },
                    VcpuRequest::Stop => break,
                };
                if responses.send((self.index, sequence, response)).is_err() {
                    break;
                }
                continue;
            }
            if let RunOutcome::Exit(reason) = self.run_once() {
                self.exit.request(reason);
                paused = true;
            }
        }
    }
}
