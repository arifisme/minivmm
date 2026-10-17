#![allow(dead_code)]

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use flate2::Compression;
use flate2::write::GzEncoder;
use reqwest::blocking::{Client, multipart};
use serde_json::{Value, json};

pub const START_TIMEOUT: Duration = Duration::from_secs(60);

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn env_path(name: &str, default: &str) -> PathBuf {
    std::env::var_os(name)
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace().join(default))
}

pub struct Artifacts {
    pub kernel: PathBuf,
    pub runtime_dir: PathBuf,
    pub vmm: PathBuf,
    pub native_function: PathBuf,
}

pub fn artifacts(test: &str) -> Option<Artifacts> {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("skipping {test}: the platform tests need root for the jail and cgroups");
        return None;
    }
    if OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .is_err()
    {
        eprintln!("skipping {test}: /dev/kvm is not available");
        return None;
    }
    let artifacts = Artifacts {
        kernel: env_path("MV_KERNEL", "guest/out/bzImage"),
        runtime_dir: env_path("MV_RUNTIME_DIR", "guest/out/runtimes"),
        vmm: env_path("MV_VMM", "target/x86_64-unknown-linux-musl/release/minivmm"),
        native_function: env_path(
            "MV_NATIVE_FUNCTION",
            "target/x86_64-unknown-linux-musl/release/bootstrap",
        ),
    };
    for path in [
        &artifacts.kernel,
        &artifacts.vmm,
        &artifacts.native_function,
        &artifacts.runtime_dir.join("native.ext4"),
        &artifacts.runtime_dir.join("python.ext4"),
    ] {
        if !path.exists() {
            eprintln!(
                "skipping {test}: {} is missing (see scripts/build-runtimes.sh)",
                path.display()
            );
            return None;
        }
    }
    Some(artifacts)
}

pub fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("platform")
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn shared_base() -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("platform-base");
    fs::create_dir_all(&dir).unwrap();
    dir
}

pub fn log_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("mvd-logs");
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

pub struct Reply {
    pub status: u16,
    pub headers: reqwest::header::HeaderMap,
    pub body: Value,
    pub elapsed: Duration,
}

impl Reply {
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    }
}

pub struct Daemon {
    pub name: String,
    pub data_dir: PathBuf,
    cgroup_root: PathBuf,
    args: Vec<String>,
    child: Option<Child>,
    pub url: String,
    client: Client,
    artifacts: Artifacts,
    log: PathBuf,
}

impl Daemon {
    pub fn start(name: &str, artifacts: Artifacts, extra: &[&str]) -> Self {
        let data_dir = scratch(name);
        std::os::unix::fs::symlink(shared_base(), data_dir.join("base")).unwrap();
        let mut daemon = Self {
            name: name.to_string(),
            data_dir,
            cgroup_root: PathBuf::from(format!("/sys/fs/cgroup/mvd-test-{name}")),
            args: extra.iter().map(|s| s.to_string()).collect(),
            child: None,
            url: String::new(),
            client: Client::builder()
                .timeout(Duration::from_secs(120))
                .build()
                .unwrap(),
            artifacts,
            log: log_dir().join(format!("{name}.log")),
        };
        daemon.launch();
        daemon
    }

    fn launch(&mut self) {
        let port = free_port();
        self.url = format!("http://127.0.0.1:{port}");
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_mvd"))
            .arg("--listen")
            .arg(format!("127.0.0.1:{port}"))
            .arg("--data-dir")
            .arg(&self.data_dir)
            .arg("--kernel")
            .arg(&self.artifacts.kernel)
            .arg("--runtime-dir")
            .arg(&self.artifacts.runtime_dir)
            .arg("--vmm")
            .arg(&self.artifacts.vmm)
            .arg("--jailer")
            .arg(env!("CARGO_BIN_EXE_mv-jailer"))
            .arg("--cgroup-root")
            .arg(&self.cgroup_root)
            .args(&self.args)
            .env("RUST_LOG", "info,mvd=debug")
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .expect("start mvd");
        self.child = Some(child);
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            if self
                .client
                .get(format!("{}/healthz", self.url))
                .send()
                .is_ok_and(|r| r.status().is_success())
            {
                return;
            }
            if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                self.fail(&format!("mvd exited during startup with {status}"));
            }
            assert!(Instant::now() < deadline, "mvd did not start");
            thread::sleep(Duration::from_millis(50));
        }
    }

    pub fn fail(&self, message: &str) -> ! {
        let log = fs::read_to_string(&self.log).unwrap_or_default();
        let tail: Vec<&str> = log.lines().rev().take(80).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        panic!(
            "{}: {message}\n--- mvd log tail ---\n{}",
            self.name,
            tail.join("\n")
        );
    }

    pub fn stop(&mut self) -> ExitStatus {
        let mut child = self.child.take().expect("daemon running");
        // SAFETY: signalling our own child.
        unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                return status;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                return child.wait().unwrap();
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    pub fn restart(&mut self) {
        let status = self.stop();
        assert!(status.success(), "mvd exited with {status}");
        self.launch();
    }

    fn reply(response: reqwest::blocking::Response, started: Instant) -> Reply {
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let text = response.text().unwrap_or_default();
        let body = serde_json::from_str(&text).unwrap_or(Value::String(text));
        Reply {
            status,
            headers,
            body,
            elapsed: started.elapsed(),
        }
    }

    pub fn get(&self, path: &str) -> Reply {
        let started = Instant::now();
        let response = self
            .client
            .get(format!("{}{path}", self.url))
            .send()
            .unwrap_or_else(|e| self.fail(&format!("GET {path}: {e}")));
        Self::reply(response, started)
    }

    pub fn delete(&self, path: &str) -> Reply {
        let started = Instant::now();
        let response = self
            .client
            .delete(format!("{}{path}", self.url))
            .send()
            .unwrap_or_else(|e| self.fail(&format!("DELETE {path}: {e}")));
        Self::reply(response, started)
    }

    pub fn deploy(&self, name: &str, config: Value, code: Vec<u8>) -> Reply {
        let started = Instant::now();
        let form = multipart::Form::new()
            .text("config", config.to_string())
            .part(
                "code",
                multipart::Part::bytes(code).file_name("code.tar.gz"),
            );
        let response = self
            .client
            .put(format!("{}/v1/functions/{name}", self.url))
            .multipart(form)
            .send()
            .unwrap_or_else(|e| self.fail(&format!("deploy {name}: {e}")));
        Self::reply(response, started)
    }

    pub fn deploy_ok(&self, name: &str, config: Value, code: Vec<u8>) -> Reply {
        let reply = self.deploy(name, config, code);
        if reply.status != 201 {
            self.fail(&format!(
                "deploy of {name} failed: {} {}",
                reply.status, reply.body
            ));
        }
        reply
    }

    pub fn invoke(&self, name: &str, event: Value) -> Reply {
        let started = Instant::now();
        let response = self
            .client
            .post(format!("{}/v1/functions/{name}/invoke", self.url))
            .body(event.to_string())
            .send()
            .unwrap_or_else(|e| self.fail(&format!("invoke {name}: {e}")));
        Self::reply(response, started)
    }

    pub fn invoke_ok(&self, name: &str, event: Value) -> Reply {
        let reply = self.invoke(name, event);
        if reply.status != 200 {
            self.fail(&format!(
                "invoke of {name} failed: {} {}",
                reply.status, reply.body
            ));
        }
        reply
    }

    pub fn vms(&self, function: &str) -> Vec<Value> {
        let reply = self.get("/v1/vms");
        reply
            .body
            .as_array()
            .unwrap()
            .iter()
            .filter(|vm| vm["function"] == function)
            .cloned()
            .collect()
    }

    pub fn metrics(&self) -> String {
        match self.get("/metrics").body {
            Value::String(text) => text,
            other => other.to_string(),
        }
    }

    pub fn cgroup_root(&self) -> &Path {
        &self.cgroup_root
    }

    /// Every jail directory and cgroup must belong to a VM the daemon reports as live.
    pub fn assert_no_leaks(&self) {
        let live = |dir: &Path| -> HashSet<String> {
            let Ok(entries) = fs::read_dir(dir) else {
                return HashSet::new();
            };
            entries
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        };
        self.wait_until(
            "jails and cgroups match the live VMs",
            Duration::from_secs(15),
            || {
                let reply = self.get("/v1/vms");
                let vms: HashSet<String> = reply
                    .body
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|vm| vm["id"].as_str().unwrap().to_string())
                    .collect();
                live(&self.data_dir.join("jails")) == vms && live(&self.cgroup_root) == vms
            },
        );
    }

    pub fn wait_until(&self, what: &str, timeout: Duration, mut check: impl FnMut() -> bool) {
        let deadline = Instant::now() + timeout;
        while !check() {
            if Instant::now() > deadline {
                self.fail(&format!("timed out waiting until {what}"));
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    pub fn native_code(&self) -> Vec<u8> {
        archive(&[(
            "bootstrap",
            fs::read(&self.artifacts.native_function).unwrap(),
            0o755,
        )])
    }

    pub fn python_code(&self, source: &str) -> Vec<u8> {
        archive(&[("main.py", source.as_bytes().to_vec(), 0o644)])
    }

    pub fn python_example(&self) -> Vec<u8> {
        self.python_code(&fs::read_to_string(workspace().join("examples/python/main.py")).unwrap())
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if self.child.is_some() {
            self.stop();
        }
        let _ = fs::remove_dir(&self.cgroup_root);
    }
}

pub fn archive(files: &[(&str, Vec<u8>, u32)]) -> Vec<u8> {
    let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
    for (path, contents, mode) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(*mode);
        header.set_cksum();
        builder
            .append_data(&mut header, path, contents.as_slice())
            .unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap()
}

pub fn native_config(extra: Value) -> Value {
    let mut config = json!({ "runtime": "native", "memory_mib": 128, "timeout_ms": 5000 });
    merge(&mut config, extra);
    config
}

pub fn python_config(extra: Value) -> Value {
    let mut config = json!({ "runtime": "python", "memory_mib": 128, "timeout_ms": 5000 });
    merge(&mut config, extra);
    config
}

fn merge(config: &mut Value, extra: Value) {
    if let (Some(config), Value::Object(extra)) = (config.as_object_mut(), extra) {
        config.extend(extra);
    }
}

pub fn mvctl() -> Option<PathBuf> {
    let path = env_path("MV_CTL", "target/release/mvctl");
    path.exists().then_some(path)
}

pub fn report(line: &str) {
    let _ = writeln!(std::io::stdout().lock(), "{line}");
}
