use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use mv_proto::agent::{ErrorKind, FunctionError, FunctionSetup, LogStream};
use mv_proto::api::{
    DeployConfig, FunctionInfo, LogEntry, PoolInfo, StatusInfo, VmInfo, VmMemory, validate_name,
};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tracing::Instrument;

use crate::admission::{Admission, Denied, MemoryReservation};
use crate::agent::LogLine;
use crate::builder::{BuildError, Builder, FunctionBuild, SnapshotFiles, restore_args};
use crate::jail::Bind;
use crate::logs::LogStore;
use crate::metrics::Metrics;
use crate::pool::{Busy, Lease, Pool};
use crate::store::{CODE_FILE, FunctionRecord, Store};
use crate::vm::{API_SOCKET, Invocation, Launcher, MicroVm, VmError, VmFailure, api_request};

const RESTORE_TIMEOUT: Duration = Duration::from_secs(10);
const MIN_REAP_INTERVAL: Duration = Duration::from_millis(50);
const MAX_REAP_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Debug)]
pub struct Settings {
    pub uid_base: u32,
    pub idle_timeout: Duration,
    pub queue_timeout: Duration,
    pub vm_overhead_mib: u64,
}

#[derive(Debug, Error)]
pub enum PlatformError {
    #[error("function {0} does not exist")]
    NotFound(String),
    #[error("{0}")]
    BadRequest(String),
    #[error("{}", .0.message)]
    Build(BuildError),
    #[error("{message}")]
    Throttled {
        reason: &'static str,
        message: String,
    },
    #[error("the function did not finish within {0:?}")]
    Timeout(Duration),
    #[error("{message}")]
    VmFailed { kind: &'static str, message: String },
    #[error("internal error: {0}")]
    Internal(String),
}

impl From<anyhow::Error> for PlatformError {
    fn from(err: anyhow::Error) -> Self {
        PlatformError::Internal(format!("{err:#}"))
    }
}

impl From<std::io::Error> for PlatformError {
    fn from(err: std::io::Error) -> Self {
        PlatformError::Internal(err.to_string())
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Start {
    Cold(Duration),
    Warm,
}

impl Start {
    pub fn label(self) -> &'static str {
        match self {
            Start::Cold(_) => "cold",
            Start::Warm => "warm",
        }
    }
}

#[derive(Debug)]
pub enum InvokeResult {
    Response(Vec<u8>),
    FunctionError(FunctionError),
}

#[derive(Debug)]
pub struct InvokeReport {
    pub request_id: String,
    pub vm_id: Option<String>,
    pub start: Option<Start>,
    pub duration: Duration,
    pub function_ms: Option<f64>,
    pub result: Result<InvokeResult, PlatformError>,
}

pub struct Deployed {
    pub record: FunctionRecord,
    pub uid: u32,
    pub snapshot: SnapshotFiles,
    pub rootfs: PathBuf,
}

pub struct FunctionEntry {
    current: RwLock<Arc<Deployed>>,
    pool: Pool<MicroVm>,
}

impl FunctionEntry {
    fn current(&self) -> Arc<Deployed> {
        self.current
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

pub struct Platform {
    settings: Settings,
    store: Store,
    launcher: Arc<Launcher>,
    builder: Builder,
    admission: Arc<Admission>,
    logs: Arc<LogStore>,
    metrics: Arc<Metrics>,
    functions: RwLock<HashMap<String, Arc<FunctionEntry>>>,
    vms: Mutex<HashMap<String, VmInfo>>,
    deploy_lock: tokio::sync::Mutex<()>,
    started: Instant,
}

fn timestamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn stream_name(stream: LogStream) -> &'static str {
    match stream {
        LogStream::Stdout => "stdout",
        LogStream::Stderr => "stderr",
        LogStream::Agent => "agent",
    }
}

fn vm_memory(pid: u32, cgroup: &str) -> Option<VmMemory> {
    let rollup = fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).ok()?;
    let field = |name: &str| {
        rollup
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|kib| kib.parse().ok())
    };
    let cgroup_bytes = (!cgroup.is_empty())
        .then(|| fs::read_to_string(PathBuf::from(cgroup).join("memory.current")).ok())
        .flatten()
        .and_then(|text| text.trim().parse().ok());
    Some(VmMemory {
        rss_kib: field("Rss:")?,
        pss_kib: field("Pss:")?,
        cgroup_bytes,
    })
}

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

impl Platform {
    pub fn new(
        settings: Settings,
        store: Store,
        launcher: Arc<Launcher>,
        builder: Builder,
        admission: Arc<Admission>,
        metrics: Arc<Metrics>,
    ) -> Arc<Self> {
        Arc::new(Self {
            settings,
            store,
            launcher,
            builder,
            admission,
            logs: Arc::new(LogStore::default()),
            metrics,
            functions: RwLock::new(HashMap::new()),
            vms: Mutex::new(HashMap::new()),
            deploy_lock: tokio::sync::Mutex::new(()),
            started: Instant::now(),
        })
    }

    fn entry(&self, name: &str) -> Result<Arc<FunctionEntry>, PlatformError> {
        self.functions
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned()
            .ok_or_else(|| PlatformError::NotFound(name.to_string()))
    }

    fn entries(&self) -> Vec<(String, Arc<FunctionEntry>)> {
        self.functions
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(name, entry)| (name.clone(), entry.clone()))
            .collect()
    }

    fn uid_for(&self, id: i64) -> u32 {
        self.settings.uid_base + 1 + id as u32
    }

    fn deployed(&self, record: FunctionRecord) -> Deployed {
        Deployed {
            uid: self.uid_for(record.id),
            snapshot: SnapshotFiles::in_dir(&self.store.revision_dir(record.id, record.revision)),
            rootfs: self.builder.images().rootfs(record.config.runtime),
            record,
        }
    }

    fn function_log(&self, function: &str, request_id: Option<&str>, stream: &str, line: &str) {
        self.logs.push(function, request_id, stream, line);
    }

    pub async fn recover(self: &Arc<Self>) -> anyhow::Result<()> {
        for record in self.store.list()? {
            let name = record.name.clone();
            match self.restore_function(record).await {
                Ok(deployed) => {
                    let max = deployed.record.config.max_concurrency;
                    let entry = Arc::new(FunctionEntry {
                        current: RwLock::new(Arc::new(deployed)),
                        pool: Pool::new(max),
                    });
                    self.functions
                        .write()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(name.clone(), entry);
                    tracing::info!(function = %name, "function restored");
                }
                Err(err) => tracing::error!(function = %name, "cannot restore function: {err:#}"),
            }
        }
        Ok(())
    }

    async fn restore_function(&self, mut record: FunctionRecord) -> anyhow::Result<Deployed> {
        let dir = self.store.revision_dir(record.id, record.revision);
        let files = SnapshotFiles::in_dir(&dir);
        let identity = self
            .builder
            .images()
            .rootfs_identity(record.config.runtime)?;
        if files.exist() && identity == record.rootfs_identity {
            return Ok(self.deployed(record));
        }
        tracing::warn!(function = %record.name, "runtime image changed, rebuilding the function snapshot");
        let code = fs::read(dir.join(CODE_FILE))?;
        let _ = fs::remove_file(&files.state);
        let _ = fs::remove_file(&files.memory);
        let output = self
            .builder
            .function(self.function_build(&record, &code, &dir))
            .await
            .map_err(|e| anyhow::anyhow!("rebuild failed: {}", e.message))?;
        record.rootfs_identity = output.rootfs_identity;
        record.init_duration_ms = output.init_duration_ms;
        self.store.save(&record)?;
        Ok(self.deployed(record))
    }

    fn function_build<'a>(
        &self,
        record: &FunctionRecord,
        code: &'a [u8],
        out_dir: &'a std::path::Path,
    ) -> FunctionBuild<'a> {
        FunctionBuild {
            setup: FunctionSetup {
                name: record.name.clone(),
                runtime: record.config.runtime,
                handler: record.config.handler().to_string(),
                memory_mib: record.config.memory_mib,
                env: record.config.env.clone(),
            },
            vcpus: record.config.vcpus,
            uid: self.uid_for(record.id),
            code,
            out_dir,
        }
    }

    pub async fn deploy(
        self: &Arc<Self>,
        name: &str,
        config: DeployConfig,
        code: Vec<u8>,
    ) -> Result<FunctionInfo, PlatformError> {
        validate_name(name).map_err(PlatformError::BadRequest)?;
        config.validate().map_err(PlatformError::BadRequest)?;
        if code.is_empty() {
            return Err(PlatformError::BadRequest(
                "the code archive is empty".into(),
            ));
        }
        let _guard = self.deploy_lock.lock().await;
        let existing = self.store.get(name)?;
        let id = match &existing {
            Some(record) => record.id,
            None => self.store.allocate_id()?,
        };
        let now = timestamp();
        let mut record = FunctionRecord {
            id,
            name: name.to_string(),
            revision: existing.as_ref().map_or(1, |r| r.revision + 1),
            config,
            code_sha256: sha256_hex(&code),
            code_size: code.len() as u64,
            init_duration_ms: 0.0,
            rootfs_identity: String::new(),
            created_at: existing
                .as_ref()
                .map_or_else(|| now.clone(), |r| r.created_at.clone()),
            updated_at: now,
        };
        let dir = self.store.revision_dir(id, record.revision);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir)?;
        fs::write(dir.join(CODE_FILE), &code)?;
        let started = Instant::now();
        let built = self
            .builder
            .function(self.function_build(&record, &code, &dir))
            .instrument(tracing::info_span!("deploy", function = %name, revision = record.revision))
            .await;
        let output = match built {
            Ok(output) => output,
            Err(err) => {
                let _ = fs::remove_dir_all(&dir);
                for line in &err.logs {
                    self.function_log(name, None, "init", line);
                }
                self.function_log(
                    name,
                    None,
                    "platform",
                    &format!("deploy failed: {}", err.message),
                );
                self.metrics
                    .deployments
                    .with_label_values(&["failed"])
                    .inc();
                tracing::warn!(function = %name, "deploy failed: {}", err.message);
                return Err(PlatformError::Build(err));
            }
        };
        for line in &output.logs {
            self.function_log(name, None, stream_name(line.stream), &line.line);
        }
        record.init_duration_ms = output.init_duration_ms;
        record.rootfs_identity = output.rootfs_identity;
        self.store.save(&record)?;
        let revision = record.revision;
        let deployed = Arc::new(self.deployed(record));
        let max = deployed.record.config.max_concurrency;
        let replaced = {
            let mut functions = self.functions.write().unwrap_or_else(|e| e.into_inner());
            match functions.get(name) {
                Some(entry) => {
                    *entry.current.write().unwrap_or_else(|e| e.into_inner()) = deployed;
                    entry.pool.set_max(max);
                    entry.pool.take_all_idle()
                }
                None => {
                    functions.insert(
                        name.to_string(),
                        Arc::new(FunctionEntry {
                            current: RwLock::new(deployed),
                            pool: Pool::new(max),
                        }),
                    );
                    Vec::new()
                }
            }
        };
        for vm in replaced {
            self.destroy_later(vm);
        }
        self.remove_old_revisions(id, revision);
        self.metrics.deployments.with_label_values(&["ok"]).inc();
        let message = format!(
            "deployed revision {revision} in {} ms (init {:.1} ms)",
            started.elapsed().as_millis(),
            output.init_duration_ms
        );
        tracing::info!(function = %name, "{message}");
        self.function_log(name, None, "platform", &message);
        self.describe(name)
    }

    fn remove_old_revisions(&self, id: i64, keep: u64) {
        let Ok(entries) = fs::read_dir(self.store.function_dir(id)) else {
            return;
        };
        let current = format!("rev-{keep}");
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy() != current {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    }

    pub async fn delete(self: &Arc<Self>, name: &str) -> Result<(), PlatformError> {
        let _guard = self.deploy_lock.lock().await;
        let entry = self
            .functions
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name)
            .ok_or_else(|| PlatformError::NotFound(name.to_string()))?;
        let id = entry.current().record.id;
        self.store.delete(name)?;
        for vm in entry.pool.close() {
            self.destroy_now(vm).await;
        }
        let _ = fs::remove_dir_all(self.store.function_dir(id));
        self.logs.forget(name);
        for state in ["idle", "busy", "starting"] {
            let _ = self.metrics.pool.remove_label_values(&[name, state]);
        }
        tracing::info!(function = %name, "function deleted");
        Ok(())
    }

    fn info(&self, name: &str, entry: &FunctionEntry) -> FunctionInfo {
        let deployed = entry.current();
        let record = &deployed.record;
        FunctionInfo {
            name: name.to_string(),
            revision: record.revision,
            config: record.config.clone(),
            code_size: record.code_size,
            code_sha256: record.code_sha256.clone(),
            created_at: record.created_at.clone(),
            updated_at: record.updated_at.clone(),
            init_duration_ms: record.init_duration_ms,
            pool: entry.pool.info(),
        }
    }

    pub fn describe(&self, name: &str) -> Result<FunctionInfo, PlatformError> {
        let entry = self.entry(name)?;
        Ok(self.info(name, &entry))
    }

    pub fn list(&self) -> Vec<FunctionInfo> {
        let mut functions: Vec<FunctionInfo> = self
            .entries()
            .iter()
            .map(|(name, entry)| self.info(name, entry))
            .collect();
        functions.sort_by(|a, b| a.name.cmp(&b.name));
        functions
    }

    pub fn logs(
        &self,
        name: &str,
        limit: usize,
        request_id: Option<&str>,
    ) -> Result<Vec<LogEntry>, PlatformError> {
        self.entry(name)?;
        Ok(self.logs.recent(name, limit, request_id))
    }

    pub fn status(&self) -> StatusInfo {
        let mut vms = PoolInfo::default();
        let entries = self.entries();
        for (_, entry) in &entries {
            let info = entry.pool.info();
            vms.idle += info.idle;
            vms.busy += info.busy;
            vms.starting += info.starting;
        }
        StatusInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            uptime_s: self.started.elapsed().as_secs(),
            functions: entries.len() as u32,
            vms,
            capacity: self.admission.capacity(),
        }
    }

    pub fn vms(&self) -> Vec<VmInfo> {
        let mut vms: Vec<VmInfo> = self
            .vms
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        for vm in &mut vms {
            vm.memory = vm.pid.and_then(|pid| vm_memory(pid, &vm.cgroup));
        }
        vms.sort_by(|a, b| a.id.cmp(&b.id));
        vms
    }

    pub fn render_metrics(&self) -> String {
        for (name, entry) in self.entries() {
            let info = entry.pool.info();
            for (state, count) in [
                ("idle", info.idle),
                ("busy", info.busy),
                ("starting", info.starting),
            ] {
                self.metrics
                    .pool
                    .with_label_values(&[name.as_str(), state])
                    .set(i64::from(count));
            }
        }
        let capacity = self.admission.capacity();
        let mib = |value: u64| (value << 20) as i64;
        self.metrics
            .memory_limit
            .set(mib(capacity.memory_limit_mib));
        self.metrics
            .memory_reserved
            .set(mib(capacity.memory_reserved_mib));
        self.metrics
            .memory_available
            .set(mib(capacity.memory_available_mib));
        self.metrics.vcpus_busy.set(i64::from(capacity.vcpus_busy));
        self.metrics.vcpu_limit.set(i64::from(capacity.vcpu_limit));
        self.metrics.render()
    }

    fn track(&self, vm: &MicroVm, state: &str) {
        let process = vm.process();
        let info = VmInfo {
            id: vm.id().to_string(),
            function: vm.function.clone(),
            revision: vm.revision,
            state: state.to_string(),
            pid: process.pid(),
            uid: vm.uid,
            cgroup: process.cgroup_path(),
            jail: process.jail_path(),
            invocations: vm.invocations,
            memory: None,
        };
        self.vms
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(info.id.clone(), info);
    }

    async fn destroy_now(&self, vm: MicroVm) {
        self.vms
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(vm.id());
        tracing::debug!(vm = vm.id(), "destroying VM");
        vm.destroy().await;
    }

    fn destroy_later(self: &Arc<Self>, vm: MicroVm) {
        let platform = self.clone();
        tokio::spawn(async move { platform.destroy_now(vm).await });
    }

    fn evict_oldest_idle(&self) -> Option<MicroVm> {
        let (_, entry) = self
            .entries()
            .into_iter()
            .filter_map(|(name, entry)| {
                entry.pool.oldest_idle().map(|since| (since, (name, entry)))
            })
            .min_by_key(|(since, _)| *since)?
            .1;
        entry.pool.take_oldest_idle()
    }

    async fn reserve_memory(&self, mib: u64) -> Result<MemoryReservation, PlatformError> {
        loop {
            let denied = match self.admission.reserve_memory(mib) {
                Ok(reservation) => return Ok(reservation),
                Err(denied) => denied,
            };
            let Some(victim) = self.evict_oldest_idle() else {
                return Err(PlatformError::Throttled {
                    reason: denied.reason(),
                    message: match denied {
                        Denied::HostMemory => "the host is low on free memory".into(),
                        _ => "not enough memory to start another VM".into(),
                    },
                });
            };
            tracing::info!(vm = victim.id(), "evicting an idle VM to free memory");
            self.destroy_now(victim).await;
        }
    }

    async fn launch(
        &self,
        target: &Deployed,
        memory: MemoryReservation,
    ) -> Result<MicroVm, VmError> {
        let config = &target.record.config;
        let binds = [
            Bind::new(&target.snapshot.state, "/snapshot.state"),
            Bind::new(&target.snapshot.memory, "/snapshot.mem"),
            Bind::new(&target.rootfs, "/rootfs.ext4"),
        ];
        let id = self.launcher.next_id(&target.record.name);
        let started = Instant::now();
        let limits = self.launcher.limits(config.memory_mib, config.vcpus);
        let mut process = self
            .launcher
            .spawn(&id, target.uid, limits, &binds, |jail| {
                restore_args(jail, &binds, "warn")
            })?;
        tracing::debug!(vm = %id, spawn_us = started.elapsed().as_micros() as u64, "VMM spawned");
        match self
            .launcher
            .synchronize(&mut process, Instant::now() + RESTORE_TIMEOUT)
            .await
        {
            Ok(agent) => {
                let restore = started.elapsed();
                if tracing::enabled!(tracing::Level::DEBUG) {
                    let status =
                        api_request(&process.host_file(API_SOCKET), r#"{"cmd":"status"}"#).await;
                    tracing::debug!(vm = %id, total_us = restore.as_micros() as u64, "VMM status after restore: {status:?}");
                }
                self.metrics
                    .restore
                    .with_label_values(&[config.runtime.name()])
                    .observe(restore.as_secs_f64());
                Ok(MicroVm::new(
                    &target.record.name,
                    target.record.revision,
                    target.uid,
                    process,
                    agent,
                    restore,
                    memory,
                ))
            }
            Err(err) => {
                let console = process.final_console().await;
                process.destroy().await;
                Err(match err {
                    VmError::Exited { reason, .. } => VmError::Exited { reason, console },
                    other => other,
                })
            }
        }
    }

    async fn cold_start(&self, target: &Deployed) -> Result<MicroVm, PlatformError> {
        let started = Instant::now();
        let name = &target.record.name;
        let mib = u64::from(target.record.config.memory_mib) + self.settings.vm_overhead_mib;
        let memory = self.reserve_memory(mib).await?;
        let vm = self.launch(target, memory).await.map_err(|err| {
            for line in err.console() {
                self.function_log(name, None, "console", line);
            }
            tracing::warn!(function = %name, "cannot start a VM: {err}");
            PlatformError::VmFailed {
                kind: "launch_failed",
                message: format!("cannot start a VM: {err}"),
            }
        })?;
        self.metrics
            .cold_start
            .with_label_values(&[name.as_str()])
            .observe(started.elapsed().as_secs_f64());
        Ok(vm)
    }

    pub async fn invoke(self: &Arc<Self>, name: &str, event: Vec<u8>) -> InvokeReport {
        let started = Instant::now();
        let mut report = InvokeReport {
            request_id: uuid::Uuid::new_v4().to_string(),
            vm_id: None,
            start: None,
            duration: Duration::ZERO,
            function_ms: None,
            result: Err(PlatformError::Internal("not started".into())),
        };
        let span = tracing::info_span!("invoke", function = %name, request_id = %report.request_id);
        let result = self
            .serve(name, &event, &mut report)
            .instrument(span.clone())
            .await;
        report.duration = started.elapsed();
        self.record_outcome(name, &report, &result);
        span.in_scope(|| match &result {
            Ok(_) => tracing::info!(
                start = report.start.map_or("none", Start::label),
                duration_ms = report.duration.as_secs_f64() * 1000.0,
                "invocation finished"
            ),
            Err(err) => tracing::warn!(
                duration_ms = report.duration.as_secs_f64() * 1000.0,
                "invocation failed: {err}"
            ),
        });
        report.result = result;
        report
    }

    fn record_outcome(
        &self,
        name: &str,
        report: &InvokeReport,
        result: &Result<InvokeResult, PlatformError>,
    ) {
        let start = report.start.map_or("none", Start::label);
        let kind = match result {
            Ok(InvokeResult::Response(_)) => None,
            Ok(InvokeResult::FunctionError(_)) => Some("function_error"),
            Err(PlatformError::Throttled { reason, .. }) => {
                self.metrics
                    .throttled
                    .with_label_values(&[name, reason])
                    .inc();
                Some("throttled")
            }
            Err(PlatformError::Timeout(_)) => {
                self.metrics.timeouts.with_label_values(&[name]).inc();
                Some("timeout")
            }
            Err(PlatformError::NotFound(_)) => return,
            Err(PlatformError::VmFailed { kind, .. }) => Some(*kind),
            Err(PlatformError::BadRequest(_)) => Some("bad_request"),
            Err(_) => Some("internal"),
        };
        self.metrics
            .invocations
            .with_label_values(&[name, start])
            .inc();
        if let Some(kind) = kind {
            self.metrics.errors.with_label_values(&[name, kind]).inc();
        }
        if report.start.is_some() {
            self.metrics
                .duration
                .with_label_values(&[name, start])
                .observe(report.duration.as_secs_f64());
        }
    }

    async fn serve(
        self: &Arc<Self>,
        name: &str,
        event: &[u8],
        report: &mut InvokeReport,
    ) -> Result<InvokeResult, PlatformError> {
        let entry = self.entry(name)?;
        if serde_json::from_slice::<serde::de::IgnoredAny>(event).is_err() {
            return Err(PlatformError::BadRequest(
                "the event must be valid JSON".into(),
            ));
        }
        let target = entry.current();
        let config = &target.record.config;
        let _cpu = self
            .admission
            .reserve_cpu(u32::from(config.vcpus))
            .map_err(|_| PlatformError::Throttled {
                reason: "cpu",
                message: "every host CPU is busy".into(),
            })?;
        let mut dead = Vec::new();
        let lease = entry
            .pool
            .acquire(self.settings.queue_timeout, &mut dead)
            .await;
        for vm in dead {
            self.destroy_later(vm);
        }
        let lease = lease.map_err(|busy| match busy {
            Busy::ConcurrencyLimit => PlatformError::Throttled {
                reason: "concurrency",
                message: format!(
                    "{name} is already running {} requests",
                    config.max_concurrency
                ),
            },
            Busy::Closed => PlatformError::NotFound(name.to_string()),
        })?;
        let mut vm = match lease {
            Lease::Warm(vm) => {
                report.start = Some(Start::Warm);
                vm
            }
            Lease::Cold(permit) => match self.cold_start(&target).await {
                Ok(vm) => {
                    entry.pool.started(permit);
                    report.start = Some(Start::Cold(vm.restore_time));
                    vm
                }
                Err(err) => {
                    entry.pool.start_failed(permit);
                    return Err(err);
                }
            },
        };
        report.vm_id = Some(vm.id().to_string());
        self.track(&vm, "busy");
        let logs = self.logs.clone();
        let function = name.to_string();
        let mut on_log = move |line: LogLine| {
            logs.push(
                &function,
                line.request_id.as_deref(),
                stream_name(line.stream),
                &line.line,
            )
        };
        let timeout = Duration::from_millis(config.timeout_ms);
        let outcome = vm
            .invoke(&report.request_id, event, timeout, &mut on_log)
            .await;
        match outcome {
            Ok(invocation) => {
                if vm.revision == entry.current().record.revision {
                    self.track(&vm, "idle");
                    if let Some(vm) = entry.pool.release(vm) {
                        self.destroy_later(vm);
                    }
                } else {
                    entry.pool.discard();
                    self.destroy_later(vm);
                }
                Ok(match invocation {
                    Invocation::Response { body, duration_ms } => {
                        report.function_ms = Some(duration_ms);
                        InvokeResult::Response(body)
                    }
                    Invocation::Error { error, duration_ms } => {
                        report.function_ms = Some(duration_ms);
                        InvokeResult::FunctionError(error)
                    }
                })
            }
            Err(failure) => {
                entry.pool.discard();
                let request_id = report.request_id.as_str();
                let error = match failure {
                    VmFailure::Timeout => PlatformError::Timeout(timeout),
                    VmFailure::Crashed(reason) => {
                        for line in vm.process().console() {
                            self.function_log(name, Some(request_id), "console", &line);
                        }
                        PlatformError::VmFailed {
                            kind: "vm_crashed",
                            message: format!("the VM stopped during the invocation ({reason})"),
                        }
                    }
                    VmFailure::Runtime { kind, message } => PlatformError::VmFailed {
                        kind: match kind {
                            ErrorKind::RuntimeExited => "runtime_exited",
                            ErrorKind::ResponseTooLarge => "response_too_large",
                            _ => "agent_error",
                        },
                        message,
                    },
                    VmFailure::Agent(message) => PlatformError::VmFailed {
                        kind: "agent_error",
                        message,
                    },
                };
                self.function_log(name, Some(request_id), "platform", &error.to_string());
                self.destroy_later(vm);
                Err(error)
            }
        }
    }

    pub fn spawn_reaper(self: &Arc<Self>) {
        let platform = self.clone();
        let interval = (self.settings.idle_timeout / 4).clamp(MIN_REAP_INTERVAL, MAX_REAP_INTERVAL);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                ticker.tick().await;
                let Some(cutoff) = Instant::now().checked_sub(platform.settings.idle_timeout)
                else {
                    continue;
                };
                for (name, entry) in platform.entries() {
                    for vm in entry.pool.take_idle_since(cutoff) {
                        tracing::debug!(function = %name, vm = vm.id(), "scaling down an idle VM");
                        platform.destroy_now(vm).await;
                    }
                }
            }
        });
    }

    pub async fn shutdown(&self) {
        for (_, entry) in self.entries() {
            for vm in entry.pool.close() {
                self.destroy_now(vm).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_and_labels() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(Start::Cold(Duration::from_millis(3)).label(), "cold");
        assert_eq!(Start::Warm.label(), "warm");
        assert_eq!(stream_name(LogStream::Stderr), "stderr");
    }

    #[test]
    fn reads_process_memory() {
        let memory = vm_memory(std::process::id(), "").unwrap();
        assert!(memory.rss_kib > 0 && memory.pss_kib > 0);
        assert_eq!(memory.cgroup_bytes, None);
        assert!(vm_memory(u32::MAX, "").is_none());
    }
}
