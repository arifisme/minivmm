#![allow(dead_code)]

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::FromRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub const BOOT_TIMEOUT: Duration = Duration::from_secs(60);
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

pub fn guest_dir() -> PathBuf {
    std::env::var_os("MINIVMM_GUEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../guest/out"))
}

pub fn artifact(name: &str) -> PathBuf {
    guest_dir().join(name)
}

pub fn log_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("guest-logs");
    fs::create_dir_all(&dir).expect("create log directory");
    dir
}

pub fn scratch_file(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("scratch");
    fs::create_dir_all(&dir).expect("create scratch directory");
    dir.join(name)
}

pub fn environment_ready(test: &str, needs: &[&str]) -> bool {
    if OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .is_err()
    {
        eprintln!("skipping {test}: /dev/kvm is not available");
        return false;
    }
    for name in needs {
        if !artifact(name).exists() {
            eprintln!(
                "skipping {test}: {} is missing (run scripts/build-guest.sh)",
                artifact(name).display()
            );
            return false;
        }
    }
    true
}

pub fn tap_ready(test: &str) -> Option<String> {
    let name = std::env::var("MINIVMM_TAP").unwrap_or_else(|_| "tap0".to_string());
    if Path::new("/sys/class/net").join(&name).exists() {
        Some(name)
    } else {
        eprintln!("skipping {test}: tap device {name} does not exist (run scripts/setup-tap.sh)");
        None
    }
}

pub fn copy_rootfs(test: &str) -> PathBuf {
    let path = scratch_file(&format!("{test}.ext4"));
    fs::copy(artifact("rootfs.ext4"), &path).expect("copy rootfs");
    path
}

pub const ROOTFS_CMDLINE: &str = "console=ttyS0 reboot=k panic=1 root=/dev/vda rw";
pub const GUEST_IP: &str = "172.16.0.2";
pub const HOST_IP: &str = "172.16.0.1";

pub fn kernel_args(cmdline: &str, cpus: u8) -> Vec<String> {
    vec![
        "--kernel".into(),
        artifact("bzImage").display().to_string(),
        "--cmdline".into(),
        cmdline.into(),
        "--cpus".into(),
        cpus.to_string(),
        "--mem".into(),
        "256M".into(),
        "--log-level".into(),
        "info".into(),
    ]
}

pub fn rootfs_args(disk: &Path, cpus: u8) -> Vec<String> {
    let mut args = kernel_args(ROOTFS_CMDLINE, cpus);
    args.push("--disk".into());
    args.push(disk.display().to_string());
    args
}

pub fn api_request(socket: &Path, request: &str) -> String {
    let mut stream = std::os::unix::net::UnixStream::connect(socket).expect("connect to API");
    stream
        .set_read_timeout(Some(COMMAND_TIMEOUT))
        .expect("set API timeout");
    stream
        .write_all(format!("{request}\n").as_bytes())
        .expect("send API request");
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .expect("read API response");
    line
}

pub fn wait_for_path(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} did not appear",
            path.display()
        );
        thread::sleep(Duration::from_millis(50));
    }
}

pub fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_minivmm"))
}

pub struct Vm {
    pub name: String,
    child: Child,
    stdin: Option<ChildStdin>,
    output: Arc<Mutex<String>>,
    consumed: usize,
    marker: u32,
}

fn pump<R: Read + Send + 'static>(source: R, output: Arc<Mutex<String>>, mut log: File) {
    thread::spawn(move || {
        let mut reader = BufReader::new(source);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let _ = log.write_all(&buf);
                    output
                        .lock()
                        .unwrap()
                        .push_str(&String::from_utf8_lossy(&buf));
                }
            }
        }
    });
}

impl Vm {
    pub fn start(name: &str, args: &[String]) -> Self {
        Self::spawn(name, args, Stdio::piped())
    }

    pub fn start_with_stdin(name: &str, args: &[String], tty: std::os::fd::RawFd) -> Self {
        // SAFETY: duplicating a descriptor owned by the caller for the child's stdin.
        let fd = unsafe { libc::dup(tty) };
        assert!(fd >= 0, "dup failed");
        // SAFETY: fd is a fresh duplicate owned by the returned Stdio.
        Self::spawn(name, args, unsafe { Stdio::from_raw_fd(fd) })
    }

    fn spawn(name: &str, args: &[String], stdin: Stdio) -> Self {
        let mut child = Command::new(binary())
            .args(args)
            .stdin(stdin)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn minivmm");
        let output = Arc::new(Mutex::new(String::new()));
        let log = File::create(log_dir().join(format!("{name}.log"))).expect("create log");
        pump(
            child.stdout.take().unwrap(),
            output.clone(),
            log.try_clone().unwrap(),
        );
        pump(child.stderr.take().unwrap(), output.clone(), log);
        Self {
            name: name.to_string(),
            stdin: child.stdin.take(),
            child,
            output,
            consumed: 0,
            marker: 0,
        }
    }

    pub fn output(&self) -> String {
        self.output.lock().unwrap().clone()
    }

    pub fn wait_for(&mut self, pattern: &str, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let output = self.output.lock().unwrap();
                if let Some(pos) = output[self.consumed..].find(pattern) {
                    let start = self.consumed;
                    self.consumed += pos + pattern.len();
                    return output[start..self.consumed].to_string();
                }
            }
            if Instant::now() > deadline {
                self.fail(&format!("timed out waiting for {pattern:?}"));
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                thread::sleep(Duration::from_millis(200));
                if !self.output.lock().unwrap()[self.consumed..].contains(pattern) {
                    self.fail(&format!(
                        "exited with {status} while waiting for {pattern:?}"
                    ));
                }
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    pub fn fail(&mut self, message: &str) -> ! {
        let _ = self.child.kill();
        let output = self.output();
        let tail: String = output
            .chars()
            .rev()
            .take(6000)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        panic!("{}: {message}\n--- output tail ---\n{tail}", self.name);
    }

    pub fn send(&mut self, text: &str) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        stdin.write_all(text.as_bytes()).expect("write to guest");
        stdin.flush().expect("flush guest input");
    }

    pub fn run(&mut self, command: &str) -> String {
        self.run_with_timeout(command, COMMAND_TIMEOUT)
    }

    pub fn run_with_timeout(&mut self, command: &str, timeout: Duration) -> String {
        self.marker += 1;
        let marker = format!("CMD{}DONE", self.marker);
        self.send(&format!("{command}; echo CMD{}''DONE rc=$?\n", self.marker));
        let text = self.wait_for(&format!("{marker} rc="), timeout);
        let rc_line = self.wait_for("\n", timeout);
        if rc_line.trim() != "0" {
            self.fail(&format!(
                "command {command:?} failed with rc={}",
                rc_line.trim()
            ));
        }
        text
    }

    pub fn wait_exit(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                return status;
            }
            if Instant::now() > deadline {
                self.fail("did not exit in time");
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn memory_usage_kib(pid: u32) -> (u64, u64) {
    let rollup = fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).expect("smaps_rollup");
    let field = |name: &str| {
        rollup
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| panic!("{name} missing from smaps_rollup"))
    };
    (field("Rss:"), field("Pss:"))
}
