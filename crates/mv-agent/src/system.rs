use std::collections::HashMap;
use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

const RNDADDENTROPY: libc::Ioctl = 0x4008_5203;
const RNDRESEEDCRNG: libc::Ioctl = 0x5207;
const REAPER_IDLE: Duration = Duration::from_millis(50);

fn check(result: libc::c_int) -> io::Result<()> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn mount(
    source: &str,
    target: &str,
    fstype: &str,
    flags: libc::c_ulong,
    data: &str,
) -> io::Result<()> {
    fs::create_dir_all(target)?;
    let source = CString::new(source)?;
    let target_c = CString::new(target)?;
    let fstype = CString::new(fstype)?;
    let data = CString::new(data)?;
    // SAFETY: every pointer refers to a NUL-terminated string that outlives the call.
    let result = unsafe {
        libc::mount(
            source.as_ptr(),
            target_c.as_ptr(),
            fstype.as_ptr(),
            flags,
            data.as_ptr().cast(),
        )
    };
    match check(result) {
        Err(err) if err.raw_os_error() == Some(libc::EBUSY) => Ok(()),
        other => other,
    }
}

pub fn init_guest() -> io::Result<()> {
    let hardened = libc::MS_NOSUID | libc::MS_NODEV;
    mount("proc", "/proc", "proc", hardened | libc::MS_NOEXEC, "")?;
    mount("sysfs", "/sys", "sysfs", hardened | libc::MS_NOEXEC, "")?;
    mount("devtmpfs", "/dev", "devtmpfs", libc::MS_NOSUID, "mode=0755")?;
    mount("tmpfs", "/dev/shm", "tmpfs", hardened, "mode=1777")?;
    mount("tmpfs", "/tmp", "tmpfs", hardened, "mode=1777")?;
    mount("tmpfs", "/run", "tmpfs", hardened, "mode=0755")?;
    let hostname = b"minivmm";
    // SAFETY: the buffer is valid for its length.
    check(unsafe { libc::sethostname(hostname.as_ptr().cast(), hostname.len()) })
}

pub fn add_entropy(seed: &[u8]) -> io::Result<()> {
    let random = OpenOptions::new().write(true).open("/dev/urandom")?;
    let mut request = Vec::with_capacity(8 + seed.len());
    request.extend_from_slice(&((seed.len() * 8) as i32).to_ne_bytes());
    request.extend_from_slice(&(seed.len() as i32).to_ne_bytes());
    request.extend_from_slice(seed);
    // SAFETY: the buffer holds a struct rand_pool_info with buf_size bytes of data.
    check(unsafe { libc::ioctl(random.as_raw_fd(), RNDADDENTROPY, request.as_ptr()) })?;
    // SAFETY: RNDRESEEDCRNG takes no argument.
    check(unsafe { libc::ioctl(random.as_raw_fd(), RNDRESEEDCRNG) })
}

pub fn set_realtime(nanoseconds: u64) -> io::Result<()> {
    let time = libc::timespec {
        tv_sec: (nanoseconds / 1_000_000_000) as _,
        tv_nsec: (nanoseconds % 1_000_000_000) as _,
    };
    // SAFETY: time is a valid timespec.
    check(unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &time) })
}

/// Collects exit statuses. As PID 1 the agent must reap every orphan, so one thread waits for
/// any child; otherwise each spawned child gets its own waiter.
pub struct Reaper {
    statuses: Mutex<HashMap<u32, i32>>,
    changed: Condvar,
    global: OnceLock<Thread>,
}

impl Reaper {
    pub fn new(reap_all: bool) -> Arc<Self> {
        let reaper = Arc::new(Self {
            statuses: Mutex::new(HashMap::new()),
            changed: Condvar::new(),
            global: OnceLock::new(),
        });
        if reap_all {
            let worker = reaper.clone();
            let thread = thread::spawn(move || {
                loop {
                    let mut status = 0;
                    // SAFETY: waiting for any child with a valid status pointer.
                    let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
                    if pid > 0 {
                        worker.record(pid as u32, status);
                    } else {
                        thread::park_timeout(REAPER_IDLE);
                    }
                }
            });
            let _ = reaper.global.set(thread.thread().clone());
        }
        reaper
    }

    fn record(&self, pid: u32, status: i32) {
        let mut statuses = self.statuses.lock().unwrap_or_else(|e| e.into_inner());
        statuses.insert(pid, status);
        self.changed.notify_all();
    }

    pub fn watch(self: &Arc<Self>, mut child: std::process::Child) {
        let pid = child.id();
        match self.global.get() {
            Some(thread) => thread.unpark(),
            None => {
                let reaper = self.clone();
                thread::spawn(move || {
                    use std::os::unix::process::ExitStatusExt;
                    if let Ok(status) = child.wait() {
                        reaper.record(pid, status.into_raw());
                    }
                });
            }
        }
    }

    pub fn wait(&self, pid: u32, timeout: Duration) -> Option<i32> {
        let deadline = Instant::now() + timeout;
        let mut statuses = self.statuses.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(status) = statuses.remove(&pid) {
                return Some(status);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            statuses = self
                .changed
                .wait_timeout(statuses, remaining)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

pub fn describe_status(status: i32) -> String {
    if libc::WIFEXITED(status) {
        format!("exited with status {}", libc::WEXITSTATUS(status))
    } else if libc::WIFSIGNALED(status) {
        format!("killed by signal {}", libc::WTERMSIG(status))
    } else {
        format!("stopped with status {status:#x}")
    }
}
