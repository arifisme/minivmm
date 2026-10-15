mod builder;

use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use kvm_ioctls::{Cap, VmFd};
use log::{error, info, warn};
use seccompiler::BpfProgram;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use vmm_sys_util::signal::Killable;

use crate::api::{ApiCommand, ApiServer, error_response, parse_command};
use crate::boot::bzimage::BootError;
use crate::devices::acpi_pm::AcpiPm;
use crate::devices::bus::{Bus, BusError};
use crate::devices::i8042::I8042;
use crate::devices::pci::root::PciConfigIo;
use crate::devices::serial::Serial;
use crate::devices::virtio::VirtioDevice;
use crate::devices::virtio::block::Block;
use crate::devices::virtio::net::Net;
use crate::devices::virtio::pci::{VirtioPciDevice, VirtioPciState};
use crate::event_loop::EventLoop;
use crate::exit::{ExitOnPanic, ExitReason, ExitSignal};
use crate::json::Json;
use crate::kvm::KvmError;
use crate::kvm::irq::GsiRouter;
use crate::kvm::vcpu::{BootSetup, Vcpu, VcpuRequest, VcpuResponse, VcpuState};
use crate::kvm::vm::{restore_vm_state, save_vm_state};
use crate::memory::{GuestMemory, MemoryError};
use crate::seccomp::{
    SeccompError, ThreadKind, apply as apply_filter, build_filter, install_violation_reporter,
};
use crate::snapshot::{SnapshotError, SnapshotReader, VmSnapshot, write_snapshot};
use crate::sync::LockExt;
use crate::sys::signal::{SignalFd, block_signals, kick_signal, register_kick_handler};
use crate::sys::terminal::{RawModeGuard, is_terminal, read_fd};
use builder::{Machine, Shared, build_machine, load_guest};

const VCPU_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const STDIN_RETRY_MS: i32 = 10;

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
    #[error(transparent)]
    Snapshot(#[from] SnapshotError),
    #[error("cannot open {what}: {source}")]
    OpenDevice { what: String, source: io::Error },
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("vCPU {0} did not respond")]
    VcpuUnresponsive(u8),
    #[error("vCPU {index} failed: {message}")]
    VcpuFailed { index: u8, message: String },
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),
    #[error(transparent)]
    Seccomp(#[from] SeccompError),
}

pub type Result<T> = std::result::Result<T, VmmError>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskConfig {
    pub path: PathBuf,
    pub read_only: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetConfig {
    pub tap: String,
    pub mac: [u8; 6],
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineConfig {
    pub cpus: u8,
    pub mem_size: u64,
    pub disk: Option<DiskConfig>,
    pub net: Option<NetConfig>,
}

#[derive(Clone, Debug)]
pub struct BootConfig {
    pub kernel: PathBuf,
    pub initrd: Option<PathBuf>,
    pub cmdline: String,
}

#[derive(Clone, Debug, Default)]
pub struct RuntimeOptions {
    pub api_socket: Option<PathBuf>,
    pub seccomp: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeviceKind {
    Block,
    Net,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Token {
    Exit,
    Signal,
    Stdin,
    ApiListener,
    Tap,
    Queue(DeviceKind, u16),
    ApiClient(u32),
}

const QUEUE_TOKEN_BLOCK: u64 = 0x100;
const QUEUE_TOKEN_NET: u64 = 0x200;
const QUEUE_TOKEN_MASK: u64 = 0xff;
const API_CLIENT_TOKEN: u64 = 1 << 32;

impl Token {
    fn encode(self) -> u64 {
        match self {
            Token::Exit => 0,
            Token::Signal => 1,
            Token::Stdin => 2,
            Token::ApiListener => 3,
            Token::Tap => 4,
            Token::Queue(DeviceKind::Block, index) => QUEUE_TOKEN_BLOCK | u64::from(index),
            Token::Queue(DeviceKind::Net, index) => QUEUE_TOKEN_NET | u64::from(index),
            Token::ApiClient(id) => API_CLIENT_TOKEN | u64::from(id),
        }
    }

    fn decode(raw: u64) -> Option<Self> {
        let queue_index = (raw & QUEUE_TOKEN_MASK) as u16;
        match raw {
            0 => Some(Token::Exit),
            1 => Some(Token::Signal),
            2 => Some(Token::Stdin),
            3 => Some(Token::ApiListener),
            4 => Some(Token::Tap),
            r if r & !QUEUE_TOKEN_MASK == QUEUE_TOKEN_BLOCK => {
                Some(Token::Queue(DeviceKind::Block, queue_index))
            }
            r if r & !QUEUE_TOKEN_MASK == QUEUE_TOKEN_NET => {
                Some(Token::Queue(DeviceKind::Net, queue_index))
            }
            r if r & API_CLIENT_TOKEN != 0 && r >> 33 == 0 => Some(Token::ApiClient(r as u32)),
            _ => None,
        }
    }
}

struct VcpuHandle {
    thread: JoinHandle<()>,
    requests: Sender<(u64, VcpuRequest)>,
}

pub struct Vmm {
    config: MachineConfig,
    vm: Arc<VmFd>,
    memory: Arc<GuestMemory>,
    exit: Arc<ExitSignal>,
    router: Shared<GsiRouter>,
    serial: Shared<Serial>,
    i8042: Shared<I8042>,
    acpi_pm: Shared<AcpiPm>,
    pci: Shared<PciConfigIo>,
    block: Option<Shared<VirtioPciDevice<Block>>>,
    net: Option<Shared<VirtioPciDevice<Net>>>,
    vcpus: Vec<VcpuHandle>,
    responses: Receiver<(u8, u64, VcpuResponse)>,
    request_sequence: u64,
    paused: bool,
    signals: SignalFd,
    event_loop: EventLoop,
    api: Option<ApiServer>,
    stdin_registered: bool,
    stdin_open: bool,
    tap_registered: bool,
    vmm_filter: Option<BpfProgram>,
}

fn prepare_process() -> Result<()> {
    block_signals(&[libc::SIGINT, libc::SIGTERM])?;
    register_kick_handler()?;
    Ok(())
}

impl Vmm {
    pub fn boot(
        config: MachineConfig,
        boot: &BootConfig,
        options: &RuntimeOptions,
    ) -> Result<Self> {
        prepare_process()?;
        let machine = build_machine(&config)?;
        let kernel = load_guest(boot, &config, &machine.memory)?;
        let setup = BootSetup {
            memory: &machine.memory,
            entry: kernel.entry,
            zero_page: crate::arch::layout::ZERO_PAGE_START,
        };
        for vcpu in &machine.vcpus {
            let bsp = (vcpu.index() == 0).then_some(&setup);
            vcpu.configure(&machine.kvm, config.cpus, bsp)?;
        }
        info!(
            "booting {} with {} vCPUs and {} MiB",
            boot.kernel.display(),
            config.cpus,
            config.mem_size >> 20
        );
        Self::start(config, machine, options)
    }

    pub fn restore(path: &Path, options: &RuntimeOptions) -> Result<Self> {
        prepare_process()?;
        let (snapshot, reader) = SnapshotReader::open(path)?;
        if snapshot.vcpus.len() != usize::from(snapshot.config.cpus) {
            return Err(SnapshotError::Corrupt("vCPU count mismatch".into()).into());
        }
        let config = snapshot.config.clone();
        let machine = build_machine(&config)?;
        reader.load_memory(&machine.memory)?;
        restore_vm_state(&machine.vm, &snapshot.vm)?;
        machine.router.lock_or_recover().restore(snapshot.routing)?;
        machine.serial.lock_or_recover().restore(snapshot.serial);
        machine.i8042.lock_or_recover().restore(snapshot.i8042);
        machine.acpi_pm.lock_or_recover().restore(snapshot.acpi_pm);
        if !machine.pci.lock_or_recover().restore(&snapshot.pci) {
            return Err(SnapshotError::Corrupt("PCI host bridge state mismatch".into()).into());
        }
        restore_virtio(&machine.block, snapshot.block, &machine.mmio)?;
        restore_virtio(&machine.net, snapshot.net, &machine.mmio)?;
        for (vcpu, state) in machine.vcpus.iter().zip(&snapshot.vcpus) {
            vcpu.restore_state(state)?;
        }
        info!("restored {} vCPUs from {}", config.cpus, path.display());
        let vmm = Self::start(config, machine, options)?;
        vmm.kick_all_queues();
        Ok(vmm)
    }

    fn start(config: MachineConfig, machine: Machine, options: &RuntimeOptions) -> Result<Self> {
        let signals = SignalFd::new(&[libc::SIGINT, libc::SIGTERM])?;
        let event_loop = EventLoop::new()?;
        event_loop.add(machine.exit.event().as_raw_fd(), Token::Exit.encode())?;
        event_loop.add(signals.as_raw_fd(), Token::Signal.encode())?;
        event_loop.add(io::stdin().as_raw_fd(), Token::Stdin.encode())?;
        register_queues(&event_loop, &machine.block, DeviceKind::Block)?;
        register_queues(&event_loop, &machine.net, DeviceKind::Net)?;
        let api = match &options.api_socket {
            Some(path) => {
                let server = ApiServer::bind(path).map_err(|source| VmmError::OpenDevice {
                    what: format!("API socket {}", path.display()),
                    source,
                })?;
                event_loop.add(server.listener_fd(), Token::ApiListener.encode())?;
                Some(server)
            }
            None => None,
        };

        let (vmm_filter, vcpu_filter) = if options.seccomp {
            install_violation_reporter()?;
            (
                Some(build_filter(ThreadKind::Vmm)?),
                Some(Arc::new(build_filter(ThreadKind::Vcpu)?)),
            )
        } else {
            (None, None)
        };
        let (response_tx, responses) = mpsc::channel();
        let mut vcpus = Vec::new();
        for vcpu in machine.vcpus {
            vcpus.push(spawn_vcpu(
                vcpu,
                response_tx.clone(),
                vcpu_filter.clone(),
                machine.exit.clone(),
            )?);
        }
        Ok(Self {
            config,
            vm: machine.vm,
            memory: machine.memory,
            exit: machine.exit,
            router: machine.router,
            serial: machine.serial,
            i8042: machine.i8042,
            acpi_pm: machine.acpi_pm,
            pci: machine.pci,
            block: machine.block,
            net: machine.net,
            vcpus,
            responses,
            request_sequence: 0,
            paused: false,
            signals,
            event_loop,
            api,
            stdin_registered: true,
            stdin_open: true,
            tap_registered: false,
            vmm_filter,
        })
    }

    fn kick_all_queues(&self) {
        let kick = |events: &[vmm_sys_util::eventfd::EventFd]| {
            for event in events {
                let _ = event.write(1);
            }
        };
        if let Some(block) = &self.block {
            kick(block.lock_or_recover().notify_events());
        }
        if let Some(net) = &self.net {
            kick(net.lock_or_recover().notify_events());
        }
    }

    pub fn run(mut self) -> Result<ExitReason> {
        let _raw_mode = if is_terminal(io::stdin().as_raw_fd()) {
            RawModeGuard::new(io::stdin().as_raw_fd()).ok()
        } else {
            None
        };
        if let Some(filter) = self.vmm_filter.take() {
            apply_filter(&filter)?;
        }
        let result = self.event_loop_body();
        self.shutdown();
        result
    }

    fn event_loop_body(&mut self) -> Result<ExitReason> {
        self.sync_tap()?;
        loop {
            if let Some(reason) = self.exit.reason() {
                return Ok(reason);
            }
            let waiting_for_stdin_room = self.stdin_open && !self.stdin_registered;
            let timeout = if waiting_for_stdin_room {
                STDIN_RETRY_MS
            } else {
                -1
            };
            let tokens = self.event_loop.wait(timeout)?;
            if waiting_for_stdin_room {
                self.resume_stdin()?;
            }
            for token in tokens.into_iter().filter_map(Token::decode) {
                self.dispatch(token)?;
            }
        }
    }

    fn dispatch(&mut self, token: Token) -> Result<()> {
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
            Token::Tap => {
                if let Some(net) = &self.net {
                    net.lock_or_recover()
                        .process(|device, memory, queues| device.receive(memory, queues));
                }
                self.sync_tap()?;
            }
            Token::Queue(kind, index) => {
                self.process_queue(kind, usize::from(index));
                if kind == DeviceKind::Net {
                    self.sync_tap()?;
                }
            }
            Token::ApiListener => self.accept_api_clients()?,
            Token::ApiClient(id) => self.handle_api_client(id)?,
        }
        Ok(())
    }

    fn process_queue(&self, kind: DeviceKind, index: usize) {
        fn run<D: VirtioDevice>(device: &Option<Shared<VirtioPciDevice<D>>>, index: usize) {
            if let Some(device) = device {
                let mut device = device.lock_or_recover();
                if let Some(event) = device.notify_events().get(index) {
                    let _ = event.read();
                }
                device.process_queue(index);
            }
        }
        match kind {
            DeviceKind::Block => run(&self.block, index),
            DeviceKind::Net => run(&self.net, index),
        }
    }

    fn sync_tap(&mut self) -> Result<()> {
        let Some(net) = &self.net else {
            return Ok(());
        };
        let (wanted, fd) = {
            let net = net.lock_or_recover();
            (
                net.is_active() && !net.device().rx_blocked(),
                net.device().backend_fd(),
            )
        };
        let Some(fd) = fd else {
            return Ok(());
        };
        if wanted && !self.tap_registered {
            self.event_loop.add(fd, Token::Tap.encode())?;
        } else if !wanted && self.tap_registered {
            self.event_loop.remove(fd)?;
        }
        self.tap_registered = wanted;
        Ok(())
    }

    fn handle_stdin(&mut self) -> Result<()> {
        let room = self.serial.lock_or_recover().input_room();
        if room == 0 {
            return self.pause_stdin();
        }
        let mut buf = vec![0u8; room];
        match read_fd(io::stdin().as_raw_fd(), &mut buf) {
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

    fn accept_api_clients(&mut self) -> Result<()> {
        let Some(api) = &mut self.api else {
            return Ok(());
        };
        for (id, fd) in api.accept() {
            self.event_loop.add(fd, Token::ApiClient(id).encode())?;
        }
        Ok(())
    }

    fn handle_api_client(&mut self, id: u32) -> Result<()> {
        let Some(api) = &mut self.api else {
            return Ok(());
        };
        let input = api.read(id);
        for request in input.requests {
            let response = match parse_command(&request) {
                Ok(command) => self.execute(command),
                Err(message) => error_response(&message),
            };
            if let Some(api) = &mut self.api {
                api.respond(id, &response);
            }
        }
        if input.closed
            && let Some(api) = &mut self.api
        {
            if let Some(fd) = api.client_fd(id) {
                let _ = self.event_loop.remove(fd);
            }
            api.remove(id);
        }
        Ok(())
    }

    fn execute(&mut self, command: ApiCommand) -> Json {
        info!("API command: {command:?}");
        let result = match command {
            ApiCommand::Status => return self.status(),
            ApiCommand::Pause => self.pause(),
            ApiCommand::Resume => self.resume(),
            ApiCommand::Snapshot(path) => self.snapshot(&path),
            ApiCommand::Shutdown => {
                self.exit.request(ExitReason::HostRequest);
                Ok(())
            }
        };
        match result {
            Ok(()) => self.status(),
            Err(err) => {
                error!("API command failed: {err}");
                error_response(&err.to_string())
            }
        }
    }

    fn status(&self) -> Json {
        let state = if self.exit.reason().is_some() {
            "stopping"
        } else if self.paused {
            "paused"
        } else {
            "running"
        };
        Json::object([
            ("ok", Json::from(true)),
            ("state", Json::from(state)),
            ("vcpus", Json::from(u64::from(self.config.cpus))),
            ("memory_mib", Json::from(self.config.mem_size >> 20)),
        ])
    }

    pub fn pause(&mut self) -> Result<()> {
        self.request_all(|| VcpuRequest::Pause)?;
        self.paused = true;
        Ok(())
    }

    pub fn resume(&mut self) -> Result<()> {
        self.request_all(|| VcpuRequest::Resume)?;
        self.paused = false;
        Ok(())
    }

    pub fn snapshot(&mut self, path: &Path) -> Result<()> {
        let was_paused = self.paused;
        self.pause()?;
        let result = self.write_snapshot(path);
        if !was_paused {
            self.resume()?;
        }
        if result.is_ok() {
            info!("snapshot written to {}", path.display());
        }
        result
    }

    fn write_snapshot(&mut self, path: &Path) -> Result<()> {
        let vcpus = self
            .request_all(|| VcpuRequest::SaveState)?
            .into_iter()
            .enumerate()
            .map(|(index, response)| match response {
                VcpuResponse::State(state) => Ok(*state),
                _ => Err(VmmError::VcpuUnresponsive(index as u8)),
            })
            .collect::<Result<Vec<VcpuState>>>()?;
        let snapshot = VmSnapshot {
            config: self.config.clone(),
            vm: save_vm_state(&self.vm)?,
            routing: self.router.lock_or_recover().state(),
            vcpus,
            serial: self.serial.lock_or_recover().state(),
            i8042: self.i8042.lock_or_recover().state(),
            acpi_pm: self.acpi_pm.lock_or_recover().state(),
            pci: self.pci.lock_or_recover().state(),
            block: self.block.as_ref().map(|d| d.lock_or_recover().state()),
            net: self.net.as_ref().map(|d| d.lock_or_recover().state()),
        };
        write_snapshot(path, &snapshot, &self.memory)?;
        Ok(())
    }

    fn request_all(&mut self, request: impl Fn() -> VcpuRequest) -> Result<Vec<VcpuResponse>> {
        self.request_sequence += 1;
        for vcpu in &self.vcpus {
            let _ = vcpu.requests.send((self.request_sequence, request()));
            let _ = vcpu.thread.kill(kick_signal());
        }
        collect_responses(
            &self.responses,
            self.request_sequence,
            self.vcpus.len(),
            VCPU_RESPONSE_TIMEOUT,
        )
    }

    fn shutdown(&mut self) {
        for vcpu in &self.vcpus {
            let _ = vcpu
                .requests
                .send((self.request_sequence, VcpuRequest::Stop));
            let _ = vcpu.thread.kill(kick_signal());
        }
        for vcpu in self.vcpus.drain(..) {
            if vcpu.thread.join().is_err() {
                error!("a vCPU thread panicked");
            }
        }
    }
}

fn collect_responses(
    responses: &Receiver<(u8, u64, VcpuResponse)>,
    sequence: u64,
    count: usize,
    timeout: Duration,
) -> Result<Vec<VcpuResponse>> {
    let deadline = Instant::now() + timeout;
    let mut collected: Vec<Option<VcpuResponse>> = (0..count).map(|_| None).collect();
    while let Some(missing) = collected.iter().position(Option::is_none) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let (index, answered, response) = responses
            .recv_timeout(remaining)
            .map_err(|_| VmmError::VcpuUnresponsive(missing as u8))?;
        if answered != sequence {
            continue;
        }
        if let VcpuResponse::Failed(message) = response {
            return Err(VmmError::VcpuFailed { index, message });
        }
        if let Some(slot) = collected.get_mut(usize::from(index)) {
            *slot = Some(response);
        }
    }
    Ok(collected.into_iter().flatten().collect())
}

fn register_queues<D: VirtioDevice>(
    event_loop: &EventLoop,
    device: &Option<Shared<VirtioPciDevice<D>>>,
    kind: DeviceKind,
) -> Result<()> {
    if let Some(device) = device {
        let device = device.lock_or_recover();
        for (index, event) in device.notify_events().iter().enumerate() {
            let fd: RawFd = event.as_raw_fd();
            event_loop.add(fd, Token::Queue(kind, index as u16).encode())?;
        }
    }
    Ok(())
}

fn restore_virtio<D: VirtioDevice<State = ()>>(
    device: &Option<Shared<VirtioPciDevice<D>>>,
    state: Option<VirtioPciState<()>>,
    mmio: &Bus,
) -> Result<()> {
    match (device, state) {
        (Some(device), Some(state)) => {
            if let Some(change) = device.lock_or_recover().restore(state)? {
                mmio.relocate(change.old, change.new)?;
            }
            Ok(())
        }
        (None, None) => Ok(()),
        _ => Err(SnapshotError::Corrupt("device set does not match configuration".into()).into()),
    }
}

fn spawn_vcpu(
    vcpu: Vcpu,
    responses: Sender<(u8, u64, VcpuResponse)>,
    filter: Option<Arc<BpfProgram>>,
    exit: Arc<ExitSignal>,
) -> Result<VcpuHandle> {
    let (requests, receiver) = mpsc::channel();
    let thread = std::thread::Builder::new()
        .name(format!("vcpu{}", vcpu.index()))
        .spawn(move || {
            let _fault_on_panic = ExitOnPanic::new(exit.clone());
            if let Some(filter) = filter
                && let Err(err) = apply_filter(&filter)
            {
                error!("vcpu {}: {err}", vcpu.index());
                exit.request(ExitReason::Fault);
                return;
            }
            vcpu.run(receiver, responses);
        })?;
    Ok(VcpuHandle { thread, requests })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_round_trip() {
        for token in [
            Token::Exit,
            Token::Signal,
            Token::Stdin,
            Token::ApiListener,
            Token::Tap,
            Token::Queue(DeviceKind::Block, 0),
            Token::Queue(DeviceKind::Net, 1),
            Token::ApiClient(0),
            Token::ApiClient(u32::MAX),
        ] {
            assert_eq!(Token::decode(token.encode()), Some(token));
        }
        assert_eq!(Token::decode(99), None);
        assert_eq!(Token::decode(u64::MAX), None);
    }

    #[test]
    fn late_responses_to_earlier_requests_are_ignored() {
        let (tx, rx) = mpsc::channel();
        tx.send((0, 6, VcpuResponse::Paused)).unwrap();
        tx.send((1, 7, VcpuResponse::Resumed)).unwrap();
        tx.send((0, 7, VcpuResponse::Resumed)).unwrap();
        let responses = collect_responses(&rx, 7, 2, Duration::from_millis(100)).unwrap();
        assert!(responses.iter().all(|r| matches!(r, VcpuResponse::Resumed)));

        tx.send((0, 8, VcpuResponse::Paused)).unwrap();
        tx.send((1, 7, VcpuResponse::Paused)).unwrap();
        assert!(matches!(
            collect_responses(&rx, 8, 2, Duration::from_millis(100)),
            Err(VmmError::VcpuUnresponsive(1))
        ));

        tx.send((1, 6, VcpuResponse::Failed("stale".into())))
            .unwrap();
        tx.send((0, 9, VcpuResponse::Paused)).unwrap();
        tx.send((1, 9, VcpuResponse::Failed("now".into()))).unwrap();
        assert!(matches!(
            collect_responses(&rx, 9, 2, Duration::from_millis(100)),
            Err(VmmError::VcpuFailed { index: 1, message }) if message == "now"
        ));
    }
}
