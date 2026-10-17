use std::collections::HashMap;
use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mv_proto::agent::{ErrorKind, FunctionSetup, Reply, Request, Runtime};
use serde::{Deserialize, Serialize};

use crate::agent::{AgentClient, LogLine};
use crate::jail::{Bind, make_world_readable};
use crate::store::{MEMORY_FILE, STATE_FILE};
use crate::vm::{API_SOCKET, Launcher, VSOCK_SOCKET, VmError, VmmProcess, api_request};

pub const BOOT_ARGS: &str =
    "console=ttyS0 reboot=k panic=-1 root=/dev/vda ro init=/sbin/mv-agent quiet";
const BOOT_TIMEOUT: Duration = Duration::from_secs(60);
const RESTORE_TIMEOUT: Duration = Duration::from_secs(10);
const LOAD_TIMEOUT: Duration = Duration::from_secs(120);
const INIT_GRACE: Duration = Duration::from_secs(5);
const OUT_STATE: &str = "out.state";
const OUT_MEMORY: &str = "out.mem";
const MANIFEST: &str = "manifest.json";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildErrorKind {
    Load,
    Init,
    Platform,
}

#[derive(Debug)]
pub struct BuildError {
    pub kind: BuildErrorKind,
    pub message: String,
    pub logs: Vec<String>,
}

impl BuildError {
    fn platform(message: impl Into<String>) -> Self {
        Self {
            kind: BuildErrorKind::Platform,
            message: message.into(),
            logs: Vec::new(),
        }
    }
}

impl From<VmError> for BuildError {
    fn from(err: VmError) -> Self {
        Self {
            kind: BuildErrorKind::Platform,
            message: err.to_string(),
            logs: err.console().to_vec(),
        }
    }
}

impl From<io::Error> for BuildError {
    fn from(err: io::Error) -> Self {
        Self::platform(err.to_string())
    }
}

#[derive(Clone, Debug)]
pub struct SnapshotFiles {
    pub state: PathBuf,
    pub memory: PathBuf,
}

impl SnapshotFiles {
    pub fn in_dir(dir: &Path) -> Self {
        Self {
            state: dir.join(STATE_FILE),
            memory: dir.join(MEMORY_FILE),
        }
    }

    pub fn exist(&self) -> bool {
        self.state.is_file() && self.memory.is_file()
    }
}

#[derive(Clone, Debug)]
pub struct Images {
    pub kernel: PathBuf,
    pub runtime_dir: PathBuf,
}

pub fn file_identity(path: &Path) -> io::Result<String> {
    let metadata = fs::metadata(path)?;
    Ok(format!(
        "{}:{}.{}",
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec()
    ))
}

impl Images {
    pub fn rootfs(&self, runtime: Runtime) -> PathBuf {
        self.runtime_dir.join(format!("{}.ext4", runtime.name()))
    }

    pub fn rootfs_identity(&self, runtime: Runtime) -> io::Result<String> {
        file_identity(&self.rootfs(runtime))
    }
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Manifest {
    kernel: String,
    rootfs: String,
    jailed: bool,
}

pub struct FunctionBuild<'a> {
    pub setup: FunctionSetup,
    pub vcpus: u8,
    pub uid: u32,
    pub code: &'a [u8],
    pub out_dir: &'a Path,
}

pub struct BuildOutput {
    pub init_duration_ms: f64,
    pub rootfs_identity: String,
    pub logs: Vec<LogLine>,
}

/// Builds runtime base snapshots (boot, agent ready) once per runtime and VM shape, and
/// function snapshots (base restored, code loaded, init run) on every deploy.
pub struct Builder {
    launcher: Arc<Launcher>,
    images: Images,
    base_dir: PathBuf,
    build_uid: u32,
    init_timeout: Duration,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

fn path_arg(path: PathBuf) -> String {
    path.display().to_string()
}

async fn snapshot(process: &VmmProcess) -> Result<(), VmError> {
    let request = serde_json::json!({
        "cmd": "snapshot",
        "path": process.vmm_file(OUT_STATE),
        "mem_path": process.vmm_file(OUT_MEMORY),
    });
    api_request(&process.host_file(API_SOCKET), &request.to_string()).await?;
    Ok(())
}

fn collect(process: &VmmProcess, dir: &Path) -> io::Result<SnapshotFiles> {
    fs::create_dir_all(dir)?;
    let files = SnapshotFiles::in_dir(dir);
    fs::rename(process.host_file(OUT_MEMORY), &files.memory)?;
    fs::rename(process.host_file(OUT_STATE), &files.state)?;
    Ok(files)
}

impl Builder {
    pub fn new(
        launcher: Arc<Launcher>,
        images: Images,
        data_dir: &Path,
        build_uid: u32,
        init_timeout: Duration,
    ) -> Self {
        Self {
            launcher,
            images,
            base_dir: data_dir.join("base"),
            build_uid,
            init_timeout,
            locks: Mutex::new(HashMap::new()),
        }
    }

    pub fn images(&self) -> &Images {
        &self.images
    }

    fn lock_for(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.locks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key.to_string())
            .or_default()
            .clone()
    }

    fn manifest(&self, runtime: Runtime) -> io::Result<Manifest> {
        Ok(Manifest {
            kernel: file_identity(&self.images.kernel)?,
            rootfs: self.images.rootfs_identity(runtime)?,
            jailed: self.launcher.jailed(),
        })
    }

    pub async fn base(
        &self,
        runtime: Runtime,
        memory_mib: u32,
        vcpus: u8,
    ) -> Result<SnapshotFiles, BuildError> {
        let key = format!("{runtime}-{memory_mib}m-{vcpus}c");
        let lock = self.lock_for(&key);
        let _guard = lock.lock().await;
        let dir = self.base_dir.join(&key);
        let files = SnapshotFiles::in_dir(&dir);
        let manifest = self.manifest(runtime)?;
        let stored = fs::read(dir.join(MANIFEST))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Manifest>(&bytes).ok());
        if files.exist() && stored.as_ref() == Some(&manifest) {
            return Ok(files);
        }
        tracing::info!(%key, "building runtime base snapshot");
        let started = Instant::now();
        let _ = fs::remove_dir_all(&dir);
        let kernel = Bind::new(&self.images.kernel, "/bzImage");
        let rootfs = Bind::new(self.images.rootfs(runtime), "/rootfs.ext4");
        let binds = [kernel.clone(), rootfs.clone()];
        let id = self.launcher.next_id(&format!("base-{runtime}"));
        let limits = self.launcher.build_limits(memory_mib, vcpus);
        let mut process = self
            .launcher
            .spawn(&id, self.build_uid, limits, &binds, |jail| {
                vec![
                    "--kernel".into(),
                    path_arg(jail.vmm_path(&kernel)),
                    "--cmdline".into(),
                    BOOT_ARGS.into(),
                    "--cpus".into(),
                    vcpus.to_string(),
                    "--mem".into(),
                    format!("{memory_mib}M"),
                    "--disk".into(),
                    path_arg(jail.vmm_path(&rootfs)),
                    "--disk-read-only".into(),
                    "--vsock-uds".into(),
                    path_arg(jail.vmm_file(VSOCK_SOCKET)),
                    "--api".into(),
                    path_arg(jail.vmm_file(API_SOCKET)),
                    "--log-level".into(),
                    "warn".into(),
                ]
            })?;
        let result = self.capture_base(&mut process, &dir, &manifest).await;
        let console = process.console();
        process.destroy().await;
        match result {
            Ok(()) => {
                tracing::info!(%key, elapsed_ms = started.elapsed().as_millis() as u64, "base snapshot ready");
                Ok(files)
            }
            Err(mut err) => {
                let _ = fs::remove_dir_all(&dir);
                if err.logs.is_empty() {
                    err.logs = console;
                }
                Err(err)
            }
        }
    }

    async fn capture_base(
        &self,
        process: &mut VmmProcess,
        dir: &Path,
        manifest: &Manifest,
    ) -> Result<(), BuildError> {
        let mut agent = self
            .launcher
            .connect(process, Instant::now() + BOOT_TIMEOUT)
            .await?;
        match agent
            .call(&Request::Health, &[], BOOT_TIMEOUT, &mut |_| {})
            .await
        {
            Ok((Reply::Healthy { .. }, _)) => {}
            Ok((other, _)) => {
                return Err(BuildError::platform(format!(
                    "unexpected health reply {other:?}"
                )));
            }
            Err(err) => return Err(BuildError::platform(err.to_string())),
        }
        drop(agent);
        snapshot(process).await?;
        let files = collect(process, dir)?;
        make_world_readable(&files.state)?;
        make_world_readable(&files.memory)?;
        fs::write(
            dir.join(MANIFEST),
            serde_json::to_vec(manifest).map_err(io::Error::other)?,
        )?;
        Ok(())
    }

    pub async fn function(&self, build: FunctionBuild<'_>) -> Result<BuildOutput, BuildError> {
        let runtime = build.setup.runtime;
        let base = self
            .base(runtime, build.setup.memory_mib, build.vcpus)
            .await?;
        let rootfs_identity = self.images.rootfs_identity(runtime)?;
        let binds = [
            Bind::new(&base.state, "/snapshot.state"),
            Bind::new(&base.memory, "/snapshot.mem"),
            Bind::new(self.images.rootfs(runtime), "/rootfs.ext4"),
        ];
        let id = self
            .launcher
            .next_id(&format!("build-{}", build.setup.name));
        let limits = self
            .launcher
            .build_limits(build.setup.memory_mib, build.vcpus);
        let mut process = self
            .launcher
            .spawn(&id, build.uid, limits, &binds, |jail| {
                restore_args(jail, &binds)
            })?;
        let mut logs = Vec::new();
        let result = self.initialize(&mut process, &build, &mut logs).await;
        let console = process.console();
        process.destroy().await;
        match result {
            Ok(init_duration_ms) => Ok(BuildOutput {
                init_duration_ms,
                rootfs_identity,
                logs,
            }),
            Err(mut err) => {
                err.logs = logs
                    .iter()
                    .map(|l| l.line.clone())
                    .chain(err.logs)
                    .collect();
                if err.kind == BuildErrorKind::Platform {
                    err.logs.extend(console);
                }
                Err(err)
            }
        }
    }

    async fn initialize(
        &self,
        process: &mut VmmProcess,
        build: &FunctionBuild<'_>,
        logs: &mut Vec<LogLine>,
    ) -> Result<f64, BuildError> {
        let mut agent = self
            .launcher
            .synchronize(process, Instant::now() + RESTORE_TIMEOUT)
            .await?;
        let mut on_log = |line: LogLine| logs.push(line);
        let load = Request::Load {
            function: build.setup.clone(),
        };
        expect(
            &mut agent,
            &load,
            build.code,
            LOAD_TIMEOUT,
            &mut on_log,
            |reply| matches!(reply, Reply::Loaded { .. }).then_some(0.0),
        )
        .await?;
        let init = Request::Init {
            timeout_ms: self.init_timeout.as_millis() as u64,
        };
        let duration_ms = expect(
            &mut agent,
            &init,
            &[],
            self.init_timeout + INIT_GRACE,
            &mut on_log,
            |reply| match reply {
                Reply::Initialized { duration_ms } => Some(*duration_ms),
                _ => None,
            },
        )
        .await?;
        drop(agent);
        snapshot(process).await?;
        let files = collect(process, build.out_dir)?;
        for file in [&files.state, &files.memory] {
            fs::set_permissions(file, fs::Permissions::from_mode(0o400))?;
        }
        Ok(duration_ms)
    }
}

pub fn restore_args(jail: &crate::jail::JailDir, binds: &[Bind; 3]) -> Vec<String> {
    vec![
        "--restore".into(),
        path_arg(jail.vmm_path(&binds[0])),
        "--restore-memory".into(),
        path_arg(jail.vmm_path(&binds[1])),
        "--vsock-uds".into(),
        path_arg(jail.vmm_file(VSOCK_SOCKET)),
        "--api".into(),
        path_arg(jail.vmm_file(API_SOCKET)),
        "--log-level".into(),
        "warn".into(),
    ]
}

async fn expect(
    agent: &mut AgentClient,
    request: &Request,
    payload: &[u8],
    timeout: Duration,
    on_log: &mut (dyn FnMut(LogLine) + Send),
    accept: impl Fn(&Reply) -> Option<f64>,
) -> Result<f64, BuildError> {
    let (reply, _) = agent
        .call(request, payload, timeout, on_log)
        .await
        .map_err(|e| BuildError::platform(e.to_string()))?;
    if let Some(value) = accept(&reply) {
        return Ok(value);
    }
    Err(match reply {
        Reply::Failed { kind, message } => BuildError {
            kind: match kind {
                ErrorKind::LoadFailed => BuildErrorKind::Load,
                ErrorKind::InitFailed | ErrorKind::Timeout => BuildErrorKind::Init,
                _ => BuildErrorKind::Platform,
            },
            message,
            logs: Vec::new(),
        },
        other => BuildError::platform(format!("unexpected agent reply {other:?}")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identities_change_with_file_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("python.ext4");
        fs::write(&path, b"one").unwrap();
        let first = file_identity(&path).unwrap();
        fs::write(&path, b"three").unwrap();
        assert_ne!(first, file_identity(&path).unwrap());
        let images = Images {
            kernel: dir.path().join("bzImage"),
            runtime_dir: dir.path().to_path_buf(),
        };
        assert_eq!(images.rootfs(Runtime::Python), path);
        assert!(images.rootfs_identity(Runtime::Native).is_err());
        let files = SnapshotFiles::in_dir(dir.path());
        assert!(!files.exist());
    }

    #[test]
    fn boot_arguments_start_the_agent_as_init() {
        assert!(BOOT_ARGS.contains("init=/sbin/mv-agent"));
        assert!(BOOT_ARGS.contains(" ro "));
    }
}
