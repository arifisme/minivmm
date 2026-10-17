use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mv_proto::agent::{AGENT_PORT, ENTROPY_LEN, ErrorKind, FunctionError, Reply, Request};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::Command;
use tokio::sync::watch;

use crate::admission::MemoryReservation;
use crate::agent::{AgentClient, AgentError, LogLine};
use crate::cgroup::{Cgroup, CgroupRoot, Limits};
use crate::jail::{Bind, JailConfig, JailDir};

pub const VSOCK_SOCKET: &str = "v.sock";
pub const API_SOCKET: &str = "api.sock";
const CONSOLE_LINES: usize = 64;
const CONSOLE_LINE_BYTES: usize = 512;
const KILL_WAIT: Duration = Duration::from_secs(5);
const SYNC_TIMEOUT: Duration = Duration::from_secs(3);
const SYNC_ATTEMPTS: u32 = 3;
const EXIT_NOTICE: Duration = Duration::from_millis(200);
const CONSOLE_DRAIN: Duration = Duration::from_millis(20);
const API_TIMEOUT: Duration = Duration::from_secs(120);
const EXTRA_PIDS: u32 = 8;

#[derive(Debug, Error)]
pub enum VmError {
    #[error("cannot start the VMM: {0}")]
    Spawn(io::Error),
    #[error("the VMM exited during startup ({reason})")]
    Exited {
        reason: String,
        console: Vec<String>,
    },
    #[error(transparent)]
    Agent(#[from] AgentError),
    #[error("unexpected agent reply: {0}")]
    Unexpected(String),
    #[error("VMM control request failed: {0}")]
    Api(String),
}

impl VmError {
    pub fn console(&self) -> &[String] {
        match self {
            VmError::Exited { console, .. } => console,
            _ => &[],
        }
    }
}

#[derive(Debug)]
pub enum VmFailure {
    Timeout,
    Crashed(String),
    Runtime { kind: ErrorKind, message: String },
    Agent(String),
}

#[derive(Debug)]
pub enum Invocation {
    Response {
        body: Vec<u8>,
        duration_ms: f64,
    },
    Error {
        error: FunctionError,
        duration_ms: f64,
    },
}

type Console = Arc<Mutex<VecDeque<String>>>;

/// A running VMM process with its jail directory and cgroup, which are removed with it.
pub struct VmmProcess {
    id: String,
    pid: Option<u32>,
    exit: watch::Receiver<Option<String>>,
    console: Console,
    jail: Option<JailDir>,
    cgroup: Option<Cgroup>,
}

fn record_console(console: &Console, line: &[u8]) {
    let mut lines = console.lock().unwrap_or_else(|e| e.into_inner());
    if lines.len() == CONSOLE_LINES {
        lines.pop_front();
    }
    let end = line.len().min(CONSOLE_LINE_BYTES);
    lines.push_back(String::from_utf8_lossy(&line[..end]).trim_end().to_string());
}

async fn capture<R: AsyncRead + Unpin>(mut source: R, console: Console) {
    let mut buffer = [0u8; 4096];
    let mut line = Vec::new();
    loop {
        match source.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                for byte in &buffer[..n] {
                    if *byte == b'\n' {
                        record_console(&console, &line);
                        line.clear();
                    } else if line.len() < CONSOLE_LINE_BYTES {
                        line.push(*byte);
                    }
                }
            }
        }
    }
    if !line.is_empty() {
        record_console(&console, &line);
    }
}

fn describe_exit(status: io::Result<std::process::ExitStatus>) -> String {
    match status {
        Ok(status) => status.to_string(),
        Err(err) => format!("could not be waited for: {err}"),
    }
}

impl VmmProcess {
    fn spawn(
        mut command: Command,
        id: String,
        jail: JailDir,
        cgroup: Option<Cgroup>,
    ) -> io::Result<Self> {
        let spawned = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(err) => {
                if let Some(cgroup) = &cgroup {
                    let _ = cgroup.remove();
                }
                jail.remove();
                return Err(err);
            }
        };
        let console = Console::default();
        if let Some(stdout) = child.stdout.take() {
            tokio::spawn(capture(stdout, console.clone()));
        }
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(capture(stderr, console.clone()));
        }
        let pid = child.id();
        let (sender, exit) = watch::channel(None);
        tokio::spawn(async move {
            let status = child.wait().await;
            let _ = sender.send(Some(describe_exit(status)));
        });
        Ok(Self {
            id,
            pid,
            exit,
            console,
            jail: Some(jail),
            cgroup,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    pub fn has_exited(&self) -> bool {
        self.exit.borrow().is_some()
    }

    pub async fn exited(&mut self) -> String {
        loop {
            if let Some(reason) = self.exit.borrow().clone() {
                return reason;
            }
            if self.exit.changed().await.is_err() {
                return "lost track of the VMM".to_string();
            }
        }
    }

    /// The console after the process had a moment to exit, so its last words are included.
    pub async fn final_console(&mut self) -> Vec<String> {
        let status = match tokio::time::timeout(EXIT_NOTICE, self.exited()).await {
            Ok(reason) => format!("the VMM {reason}"),
            Err(_) => "the VMM was still running".to_string(),
        };
        tokio::time::sleep(CONSOLE_DRAIN).await;
        let mut console = self.console();
        console.push(status);
        console
    }

    pub fn console(&self) -> Vec<String> {
        self.console
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    pub fn host_file(&self, name: &str) -> PathBuf {
        self.jail
            .as_ref()
            .map(|jail| jail.host_file(name))
            .unwrap_or_default()
    }

    pub fn vmm_file(&self, name: &str) -> PathBuf {
        self.jail
            .as_ref()
            .map(|jail| jail.vmm_file(name))
            .unwrap_or_default()
    }

    pub fn jail_path(&self) -> String {
        self.jail
            .as_ref()
            .map(|jail| jail.host_dir().display().to_string())
            .unwrap_or_default()
    }

    pub fn cgroup_path(&self) -> String {
        self.cgroup
            .as_ref()
            .map(|cgroup| cgroup.path().display().to_string())
            .unwrap_or_default()
    }

    fn kill(&self) {
        if let Some(cgroup) = &self.cgroup {
            cgroup.kill();
        }
        if let Some(pid) = self.pid
            && !self.has_exited()
        {
            // SAFETY: signalling our own child, which has not been reaped yet.
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        }
    }

    pub async fn destroy(mut self) {
        self.kill();
        let _ = tokio::time::timeout(KILL_WAIT, self.exited()).await;
        let cgroup = self.cgroup.take();
        let jail = self.jail.take();
        let _ = tokio::task::spawn_blocking(move || remove(cgroup, jail)).await;
    }
}

fn remove(cgroup: Option<Cgroup>, jail: Option<JailDir>) {
    if let Some(cgroup) = cgroup
        && let Err(err) = cgroup.remove()
    {
        tracing::warn!("cannot remove cgroup {}: {err}", cgroup.path().display());
    }
    if let Some(jail) = jail {
        jail.remove();
    }
}

impl Drop for VmmProcess {
    fn drop(&mut self) {
        if self.jail.is_none() && self.cgroup.is_none() {
            return;
        }
        self.kill();
        let cgroup = self.cgroup.take();
        let jail = self.jail.take();
        std::thread::spawn(move || remove(cgroup, jail));
    }
}

pub fn host_entropy() -> [u8; ENTROPY_LEN] {
    let mut seed = [0u8; ENTROPY_LEN];
    let mut filled = 0;
    while filled < seed.len() {
        // SAFETY: the pointer and length describe the unfilled part of `seed`.
        let n =
            unsafe { libc::getrandom(seed[filled..].as_mut_ptr().cast(), seed.len() - filled, 0) };
        if n > 0 {
            filled += n as usize;
        }
    }
    seed
}

pub fn realtime_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

pub async fn api_request(socket: &Path, request: &str) -> Result<String, VmError> {
    let exchange = async {
        let mut stream = UnixStream::connect(socket).await?;
        stream.write_all(format!("{request}\n").as_bytes()).await?;
        let mut line = String::new();
        BufReader::new(stream)
            .take(64 << 10)
            .read_line(&mut line)
            .await?;
        Ok::<_, io::Error>(line)
    };
    let response = tokio::time::timeout(API_TIMEOUT, exchange)
        .await
        .map_err(|_| VmError::Api("timed out".into()))?
        .map_err(|e| VmError::Api(e.to_string()))?;
    if response.contains(r#""ok":true"#) {
        Ok(response)
    } else {
        Err(VmError::Api(response.trim().to_string()))
    }
}

pub struct Launcher {
    jail: JailConfig,
    cgroups: Option<CgroupRoot>,
    overhead_mib: u64,
    counter: AtomicU64,
}

impl Launcher {
    pub fn new(jail: JailConfig, cgroups: Option<CgroupRoot>, overhead_mib: u64) -> Self {
        Self {
            jail,
            cgroups,
            overhead_mib,
            counter: AtomicU64::new(0),
        }
    }

    pub fn jailed(&self) -> bool {
        self.jail.enabled
    }

    pub fn next_id(&self, prefix: &str) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        format!("{prefix}-{n}")
    }

    pub fn limits(&self, memory_mib: u32, vcpus: u8) -> Limits {
        Limits {
            memory_bytes: (u64::from(memory_mib) + self.overhead_mib) << 20,
            vcpus,
            pids: u32::from(vcpus) + EXTRA_PIDS,
        }
    }

    /// Build VMs also write their memory into a snapshot, and the dirty page cache is
    /// charged to their cgroup, so they get room for a second copy of guest memory.
    pub fn build_limits(&self, memory_mib: u32, vcpus: u8) -> Limits {
        let limits = self.limits(memory_mib, vcpus);
        Limits {
            memory_bytes: limits.memory_bytes + (u64::from(memory_mib) << 20),
            ..limits
        }
    }

    pub fn spawn(
        &self,
        id: &str,
        uid: u32,
        limits: Limits,
        binds: &[Bind],
        args: impl FnOnce(&JailDir) -> Vec<String>,
    ) -> Result<VmmProcess, VmError> {
        let jail = JailDir::create(&self.jail, id, uid, uid).map_err(VmError::Spawn)?;
        let cgroup = match &self.cgroups {
            Some(root) if self.jail.enabled => match root.create(id, limits) {
                Ok(cgroup) => Some(cgroup),
                Err(err) => {
                    jail.remove();
                    return Err(VmError::Spawn(err));
                }
            },
            _ => None,
        };
        let args = args(&jail);
        let command = jail.command(&self.jail, uid, uid, cgroup.as_ref(), binds, &args);
        VmmProcess::spawn(command, id.to_string(), jail, cgroup).map_err(VmError::Spawn)
    }

    pub async fn connect(
        &self,
        process: &mut VmmProcess,
        deadline: Instant,
    ) -> Result<AgentClient, VmError> {
        let uds = process.host_file(VSOCK_SOCKET);
        tokio::select! {
            client = AgentClient::connect(&uds, AGENT_PORT, deadline) => Ok(client?),
            reason = process.exited() => Err(VmError::Exited { reason, console: process.console() }),
        }
    }

    /// Connects to a restored guest and gives it fresh entropy and the current time. A guest
    /// still processing the vsock transport reset can drop the first connection, so a
    /// disconnect is retried on a new one.
    pub async fn synchronize(
        &self,
        process: &mut VmmProcess,
        deadline: Instant,
    ) -> Result<AgentClient, VmError> {
        let mut last = None;
        for _ in 0..SYNC_ATTEMPTS {
            let mut agent = self.connect(process, deadline).await?;
            let request = Request::Sync {
                realtime_ns: realtime_ns(),
            };
            match agent
                .call(&request, &host_entropy(), SYNC_TIMEOUT, &mut |_| {})
                .await
            {
                Ok((Reply::Synced { .. }, _)) => return Ok(agent),
                Ok((other, _)) => return Err(VmError::Unexpected(format!("{other:?}"))),
                Err(err) if err.is_disconnect() => last = Some(err),
                Err(err) => return Err(err.into()),
            }
        }
        Err(last.map_or_else(
            || VmError::Unexpected("no sync attempt".into()),
            VmError::Agent,
        ))
    }
}

/// A restored function VM serving one request at a time.
pub struct MicroVm {
    pub function: String,
    pub revision: u64,
    pub uid: u32,
    pub restore_time: Duration,
    pub invocations: u64,
    process: VmmProcess,
    agent: AgentClient,
    _memory: MemoryReservation,
}

impl crate::pool::PooledVm for MicroVm {
    fn is_alive(&self) -> bool {
        !self.process.has_exited()
    }
}

fn now_ms() -> u64 {
    realtime_ns() / 1_000_000
}

impl MicroVm {
    pub fn new(
        function: &str,
        revision: u64,
        uid: u32,
        process: VmmProcess,
        agent: AgentClient,
        restore_time: Duration,
        memory: MemoryReservation,
    ) -> Self {
        Self {
            function: function.to_string(),
            revision,
            uid,
            restore_time,
            invocations: 0,
            process,
            agent,
            _memory: memory,
        }
    }

    pub fn id(&self) -> &str {
        self.process.id()
    }

    pub fn process(&self) -> &VmmProcess {
        &self.process
    }

    pub async fn invoke(
        &mut self,
        request_id: &str,
        event: &[u8],
        timeout: Duration,
        on_log: &mut (dyn FnMut(LogLine) + Send),
    ) -> Result<Invocation, VmFailure> {
        self.invocations += 1;
        let request = Request::Invoke {
            request_id: request_id.to_string(),
            deadline_ms: now_ms() + timeout.as_millis() as u64,
        };
        let outcome = tokio::select! {
            result = self.agent.call(&request, event, timeout, on_log) => result,
            reason = self.process.exited() => return Err(VmFailure::Crashed(reason)),
        };
        match outcome {
            Ok((
                Reply::Invoked {
                    error: None,
                    duration_ms,
                    ..
                },
                body,
            )) => Ok(Invocation::Response { body, duration_ms }),
            Ok((
                Reply::Invoked {
                    error: Some(error),
                    duration_ms,
                    ..
                },
                _,
            )) => Ok(Invocation::Error { error, duration_ms }),
            Ok((
                Reply::Failed {
                    kind: ErrorKind::Timeout,
                    ..
                },
                _,
            ))
            | Err(AgentError::Timeout(_)) => Err(VmFailure::Timeout),
            Ok((Reply::Failed { kind, message }, _)) => Err(VmFailure::Runtime { kind, message }),
            Ok((other, _)) => Err(VmFailure::Agent(format!("unexpected reply {other:?}"))),
            Err(err) => match tokio::time::timeout(EXIT_NOTICE, self.process.exited()).await {
                Ok(reason) => Err(VmFailure::Crashed(reason)),
                Err(_) => Err(VmFailure::Agent(err.to_string())),
            },
        }
    }

    pub async fn destroy(self) {
        self.process.destroy().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_lines_are_bounded() {
        let console = Console::default();
        for i in 0..CONSOLE_LINES + 3 {
            record_console(&console, format!("line {i}\r").as_bytes());
        }
        record_console(&console, &vec![b'x'; 10_000]);
        let lines = console.lock().unwrap();
        assert_eq!(lines.len(), CONSOLE_LINES);
        assert_eq!(lines.back().unwrap().len(), CONSOLE_LINE_BYTES);
        assert_eq!(lines[0], "line 4");
    }

    #[tokio::test]
    async fn capture_splits_streams_into_lines() {
        let console = Console::default();
        capture(&b"first\nsecond\npartial"[..], console.clone()).await;
        let lines: Vec<String> = console.lock().unwrap().iter().cloned().collect();
        assert_eq!(lines, ["first", "second", "partial"]);
    }

    #[test]
    fn entropy_differs_between_calls() {
        assert_ne!(host_entropy(), host_entropy());
        assert!(realtime_ns() > 1_700_000_000_000_000_000);
    }

    #[tokio::test]
    async fn unjailed_processes_are_supervised_and_cleaned_up() {
        let base = tempfile::tempdir().unwrap();
        let config = JailConfig {
            enabled: false,
            jailer: PathBuf::from("/nonexistent"),
            vmm: PathBuf::from("/bin/sh"),
            base_dir: base.path().to_path_buf(),
        };
        let launcher = Launcher::new(config, None, 32);
        assert_eq!(launcher.limits(128, 2).memory_bytes, 160 << 20);
        assert_eq!(launcher.build_limits(128, 2).memory_bytes, 288 << 20);
        let id = launcher.next_id("fn");
        let mut process = launcher
            .spawn(&id, 0, launcher.limits(128, 1), &[], |_| {
                vec!["-c".into(), "echo booting; echo oops >&2; exit 3".into()]
            })
            .unwrap();
        let reason = process.exited().await;
        assert!(reason.contains('3'), "{reason}");
        tokio::time::sleep(Duration::from_millis(50)).await;
        let console = process.console();
        assert!(console.contains(&"booting".to_string()), "{console:?}");
        assert!(console.contains(&"oops".to_string()));
        let jail = PathBuf::from(process.jail_path());
        assert!(jail.is_dir());
        process.destroy().await;
        assert!(!jail.exists());

        let mut sleeper = launcher
            .spawn("sleeper", 0, launcher.limits(64, 1), &[], |_| {
                vec!["-c".into(), "sleep 30".into()]
            })
            .unwrap();
        assert!(!sleeper.has_exited());
        let deadline = Instant::now() + Duration::from_millis(100);
        assert!(launcher.connect(&mut sleeper, deadline).await.is_err());
        let started = Instant::now();
        sleeper.destroy().await;
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
