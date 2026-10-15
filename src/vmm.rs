use std::fs;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use kvm_ioctls::{Cap, Kvm, VmFd};
use log::{error, info, warn};
use thiserror::Error;
use vmm_sys_util::eventfd::{EFD_NONBLOCK, EventFd};
use vmm_sys_util::signal::Killable;

use crate::arch::acpi::write_acpi_tables;
use crate::arch::layout::{
    ACPI_PM_PORT, COM_PORT_LEN, COM1_PORT, EBDA_START, HIGH_MEMORY_START, I8042_PORT,
    I8042_PORT_LEN, MMIO_GAP_END, SERIAL_IRQ, ZERO_PAGE_START, low_memory_end, ram_layout,
};
use crate::boot::bzimage::{BootError, LoadedKernel, load_bzimage};
use crate::boot::params::{E820Entry, E820Type, ZeroPage, initrd_address, write_cmdline};
use crate::devices::acpi_pm::AcpiPm;
use crate::devices::bus::{Bus, BusError};
use crate::devices::i8042::I8042;
use crate::devices::serial::Serial;
use crate::event_loop::EventLoop;
use crate::exit::{ExitReason, ExitSignal};
use crate::kvm::KvmError;
use crate::kvm::irq::GsiRouter;
use crate::kvm::vcpu::{BootSetup, Vcpu, VcpuRequest, VcpuResponse};
use crate::kvm::vm::create_vm;
use crate::memory::{GuestMemory, MemoryError};
use crate::sync::LockExt;
use crate::sys::signal::{SignalFd, block_signals, kick_signal, register_kick_handler};
use crate::sys::terminal::{RawModeGuard, is_terminal};

const VCPU_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const STDIN_RETRY_MS: i32 = 10;
const ACPI_PM_LEN: u64 = 6;

#[derive(Debug, Error)]
pub enum VmmError {
    #[error("KVM is unavailable: {0}")]
    KvmUnavailable(kvm_ioctls::Error),
    #[error("required KVM capability {0:?} is missing")]
    MissingCapability(Cap),
    #[error(transparent)]
    Kvm(#[from] KvmError),
    #[error(transparent)]
    Memory(#[from] MemoryError),
    #[error(transparent)]
    Boot(#[from] BootError),
    #[error(transparent)]
    Bus(#[from] BusError),
    #[error("failed to read {path}: {source}")]
    ReadFile { path: PathBuf, source: io::Error },
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("vCPU {0} did not respond")]
    VcpuUnresponsive(u8),
    #[error("vCPU {index} failed: {message}")]
    VcpuFailed { index: u8, message: String },
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),
}

pub type Result<T> = std::result::Result<T, VmmError>;

#[derive(Clone, Debug)]
pub struct VmConfig {
    pub kernel: PathBuf,
    pub initrd: Option<PathBuf>,
    pub cmdline: String,
    pub cpus: u8,
    pub mem_size: u64,
}

const REQUIRED_CAPS: &[Cap] = &[
    Cap::Irqchip,
    Cap::Ioeventfd,
    Cap::Irqfd,
    Cap::IrqRouting,
    Cap::UserMemory,
    Cap::SetTssAddr,
    Cap::Pit2,
    Cap::PitState2,
    Cap::AdjustClock,
    Cap::VcpuEvents,
    Cap::Xsave,
    Cap::Xcrs,
    Cap::ImmediateExit,
    Cap::ExtCpuid,
    Cap::MpState,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Token {
    Exit,
    Signal,
    Stdin,
}

impl Token {
    fn encode(self) -> u64 {
        match self {
            Token::Exit => 0,
            Token::Signal => 1,
            Token::Stdin => 2,
        }
    }

    fn decode(raw: u64) -> Option<Self> {
        match raw {
            0 => Some(Token::Exit),
            1 => Some(Token::Signal),
            2 => Some(Token::Stdin),
            _ => None,
        }
    }
}

struct VcpuHandle {
    thread: JoinHandle<()>,
    requests: Sender<VcpuRequest>,
}

pub struct Vmm {
    memory: Arc<GuestMemory>,
    _vm: Arc<VmFd>,
    exit: Arc<ExitSignal>,
    serial: Arc<Mutex<Serial>>,
    _router: Arc<Mutex<GsiRouter>>,
    vcpus: Vec<VcpuHandle>,
    responses: Receiver<(u8, VcpuResponse)>,
    signals: SignalFd,
    event_loop: EventLoop,
    stdin_registered: bool,
    stdin_open: bool,
}

fn open_kvm() -> Result<Kvm> {
    let kvm = Kvm::new().map_err(VmmError::KvmUnavailable)?;
    for cap in REQUIRED_CAPS {
        if !kvm.check_extension(*cap) {
            return Err(VmmError::MissingCapability(*cap));
        }
    }
    Ok(kvm)
}

fn read_file(path: &PathBuf) -> Result<Vec<u8>> {
    fs::read(path).map_err(|source| VmmError::ReadFile {
        path: path.clone(),
        source,
    })
}

fn e820_map(mem_size: u64) -> Vec<E820Entry> {
    let low_end = low_memory_end(mem_size);
    let mut map = vec![
        E820Entry {
            addr: 0,
            size: EBDA_START.raw(),
            kind: E820Type::Ram,
        },
        E820Entry {
            addr: EBDA_START.raw(),
            size: HIGH_MEMORY_START.raw() - EBDA_START.raw(),
            kind: E820Type::Reserved,
        },
        E820Entry {
            addr: HIGH_MEMORY_START.raw(),
            size: low_end - HIGH_MEMORY_START.raw(),
            kind: E820Type::Ram,
        },
    ];
    if mem_size > low_end {
        map.push(E820Entry {
            addr: MMIO_GAP_END,
            size: mem_size - low_end,
            kind: E820Type::Ram,
        });
    }
    map
}

fn load_kernel(config: &VmConfig, memory: &GuestMemory) -> Result<LoadedKernel> {
    let low_end = low_memory_end(config.mem_size);
    let image = read_file(&config.kernel)?;
    let kernel = load_bzimage(memory, &image, low_end)?;
    let mut zero_page = ZeroPage::new(&kernel.header);
    zero_page.set_cmdline(write_cmdline(memory, &kernel.header, &config.cmdline)?);
    if let Some(path) = &config.initrd {
        let initrd = read_file(path)?;
        let addr = initrd_address(&kernel, initrd.len(), low_end)?;
        memory.write(addr, &initrd)?;
        zero_page.set_initrd(addr, initrd.len() as u64);
    }
    for entry in e820_map(config.mem_size) {
        zero_page.add_e820(entry);
    }
    zero_page.write_to(memory, ZERO_PAGE_START)?;
    write_acpi_tables(memory, config.cpus)?;
    Ok(kernel)
}

impl Vmm {
    pub fn boot(config: &VmConfig) -> Result<Self> {
        if config.cpus == 0 {
            return Err(VmmError::InvalidConfig(
                "at least one vCPU is required".into(),
            ));
        }
        if config.mem_size < (64 << 20) {
            return Err(VmmError::InvalidConfig(
                "at least 64 MiB of memory is required".into(),
            ));
        }
        block_signals(&[libc::SIGINT, libc::SIGTERM])?;
        register_kick_handler()?;
        let kvm = open_kvm()?;
        let memory = Arc::new(GuestMemory::new(&ram_layout(config.mem_size))?);
        let vm = create_vm(&kvm, &memory)?;
        let exit = Arc::new(ExitSignal::new()?);
        let pio = Arc::new(Bus::new());
        let mmio = Arc::new(Bus::new());
        let router = Arc::new(Mutex::new(GsiRouter::new(vm.clone())));

        let serial_irq = EventFd::new(EFD_NONBLOCK)?;
        vm.register_irqfd(&serial_irq, SERIAL_IRQ)
            .map_err(|source| KvmError::Ioctl {
                op: "KVM_IRQFD",
                source,
            })?;
        let serial = Arc::new(Mutex::new(Serial::new(
            Box::new(serial_irq),
            Box::new(io::stdout()),
        )));
        pio.insert(serial.clone(), COM1_PORT, COM_PORT_LEN)?;
        pio.insert(
            Arc::new(Mutex::new(I8042::new(exit.clone()))),
            I8042_PORT,
            I8042_PORT_LEN,
        )?;
        pio.insert(
            Arc::new(Mutex::new(AcpiPm::new(exit.clone()))),
            u64::from(ACPI_PM_PORT),
            ACPI_PM_LEN,
        )?;

        let kernel = load_kernel(config, &memory)?;
        let boot = BootSetup {
            memory: &memory,
            entry: kernel.entry,
            zero_page: ZERO_PAGE_START,
        };

        let (response_tx, responses) = mpsc::channel();
        let mut vcpus = Vec::new();
        for index in 0..config.cpus {
            let vcpu = Vcpu::new(&kvm, &vm, index, pio.clone(), mmio.clone(), exit.clone())?;
            vcpu.configure(&kvm, config.cpus, (index == 0).then_some(&boot))?;
            vcpus.push(spawn_vcpu(vcpu, response_tx.clone(), false)?);
        }

        let signals = SignalFd::new(&[libc::SIGINT, libc::SIGTERM])?;
        let event_loop = EventLoop::new()?;
        event_loop.add(exit.event().as_raw_fd(), Token::Exit.encode())?;
        event_loop.add(signals.as_raw_fd(), Token::Signal.encode())?;
        event_loop.add(io::stdin().as_raw_fd(), Token::Stdin.encode())?;
        info!(
            "booting {} with {} vCPUs and {} MiB",
            config.kernel.display(),
            config.cpus,
            config.mem_size >> 20
        );
        Ok(Self {
            memory,
            _vm: vm,
            exit,
            serial,
            _router: router,
            vcpus,
            responses,
            signals,
            event_loop,
            stdin_registered: true,
            stdin_open: true,
        })
    }

    pub fn memory(&self) -> &GuestMemory {
        &self.memory
    }

    pub fn run(mut self) -> Result<ExitReason> {
        let _raw_mode = if is_terminal(io::stdin().as_raw_fd()) {
            RawModeGuard::new(io::stdin().as_raw_fd()).ok()
        } else {
            None
        };
        let reason = loop {
            if let Some(reason) = self.exit.reason() {
                break reason;
            }
            let timeout = if self.stdin_open && !self.stdin_registered {
                STDIN_RETRY_MS
            } else {
                -1
            };
            let tokens = self.event_loop.wait(timeout)?;
            if timeout > 0 {
                self.resume_stdin()?;
            }
            for token in tokens.into_iter().filter_map(Token::decode) {
                match token {
                    Token::Exit => {
                        let _ = self.exit.event().read();
                    }
                    Token::Signal => {
                        if let Some(signal) = self.signals.read_signal()? {
                            info!("received signal {signal}, shutting down");
                            self.exit.request(ExitReason::HostRequest);
                        }
                    }
                    Token::Stdin => self.handle_stdin()?,
                }
            }
        };
        self.shutdown();
        Ok(reason)
    }

    fn handle_stdin(&mut self) -> Result<()> {
        let room = self.serial.lock_or_recover().input_room();
        if room == 0 {
            self.pause_stdin()?;
            return Ok(());
        }
        let mut buf = vec![0u8; room];
        match io::stdin().lock().read(&mut buf) {
            Ok(0) => {
                self.stdin_open = false;
                self.pause_stdin()?;
            }
            Ok(n) => {
                self.serial.lock_or_recover().enqueue_input(&buf[..n]);
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => {
                warn!("stdin read failed: {err}");
                self.stdin_open = false;
                self.pause_stdin()?;
            }
        }
        Ok(())
    }

    fn pause_stdin(&mut self) -> Result<()> {
        if self.stdin_registered {
            self.event_loop.remove(io::stdin().as_raw_fd())?;
            self.stdin_registered = false;
        }
        Ok(())
    }

    fn resume_stdin(&mut self) -> Result<()> {
        if self.stdin_open
            && !self.stdin_registered
            && self.serial.lock_or_recover().input_room() > 0
        {
            self.event_loop
                .add(io::stdin().as_raw_fd(), Token::Stdin.encode())?;
            self.stdin_registered = true;
        }
        Ok(())
    }

    pub fn pause(&self) -> Result<()> {
        self.request_all(|| VcpuRequest::Pause).map(drop)
    }

    pub fn resume(&self) -> Result<()> {
        self.request_all(|| VcpuRequest::Resume).map(drop)
    }

    fn request_all(&self, request: impl Fn() -> VcpuRequest) -> Result<Vec<VcpuResponse>> {
        for vcpu in &self.vcpus {
            let _ = vcpu.requests.send(request());
            let _ = vcpu.thread.kill(kick_signal());
        }
        let mut responses: Vec<Option<VcpuResponse>> = self.vcpus.iter().map(|_| None).collect();
        for _ in 0..self.vcpus.len() {
            let (index, response) = self
                .responses
                .recv_timeout(VCPU_RESPONSE_TIMEOUT)
                .map_err(|_| VmmError::VcpuUnresponsive(0))?;
            if let VcpuResponse::Failed(message) = response {
                return Err(VmmError::VcpuFailed { index, message });
            }
            if let Some(slot) = responses.get_mut(usize::from(index)) {
                *slot = Some(response);
            }
        }
        responses
            .into_iter()
            .enumerate()
            .map(|(i, r)| r.ok_or(VmmError::VcpuUnresponsive(i as u8)))
            .collect()
    }

    fn shutdown(&mut self) {
        for vcpu in &self.vcpus {
            let _ = vcpu.requests.send(VcpuRequest::Stop);
            let _ = vcpu.thread.kill(kick_signal());
        }
        for vcpu in self.vcpus.drain(..) {
            if vcpu.thread.join().is_err() {
                error!("a vCPU thread panicked");
            }
        }
    }
}

fn spawn_vcpu(
    vcpu: Vcpu,
    responses: Sender<(u8, VcpuResponse)>,
    start_paused: bool,
) -> Result<VcpuHandle> {
    let (requests, receiver) = mpsc::channel();
    let thread = std::thread::Builder::new()
        .name(format!("vcpu{}", vcpu.index()))
        .spawn(move || vcpu.run(receiver, responses, start_paused))?;
    Ok(VcpuHandle { thread, requests })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn e820_without_gap() {
        let map = e820_map(512 << 20);
        assert_eq!(map.len(), 3);
        assert_eq!(map[2].addr + map[2].size, 512 << 20);
        assert_eq!(map[1].kind, E820Type::Reserved);
    }

    #[test]
    fn e820_with_gap() {
        let map = e820_map(5 << 30);
        assert_eq!(map.len(), 4);
        assert_eq!(map[2].addr + map[2].size, 3 << 30);
        assert_eq!((map[3].addr, map[3].size), (4 << 30, 2 << 30));
    }

    #[test]
    fn tokens_round_trip() {
        for token in [Token::Exit, Token::Signal, Token::Stdin] {
            assert_eq!(Token::decode(token.encode()), Some(token));
        }
        assert_eq!(Token::decode(99), None);
    }
}
