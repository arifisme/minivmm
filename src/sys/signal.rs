use std::cell::Cell;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::ptr::null_mut;

use kvm_bindings::kvm_run;
use libc::{c_int, c_void, siginfo_t};
use vmm_sys_util::signal::{SIGRTMIN, register_signal_handler};

thread_local! {
    static KVM_RUN: Cell<*mut kvm_run> = const { Cell::new(null_mut()) };
}

pub fn kick_signal() -> c_int {
    SIGRTMIN()
}

extern "C" fn handle_kick(_: c_int, _: *mut siginfo_t, _: *mut c_void) {
    let run = KVM_RUN.with(Cell::get);
    if !run.is_null() {
        // SAFETY: the pointer refers to this thread's mapped kvm_run structure,
        // installed by KickTarget::install and cleared when that KickTarget is dropped,
        // which its owner guarantees happens before the structure is unmapped.
        unsafe { std::ptr::addr_of_mut!((*run).immediate_exit).write_volatile(1) };
    }
}

pub fn register_kick_handler() -> io::Result<()> {
    register_signal_handler(kick_signal(), handle_kick).map_err(io::Error::from)
}

pub struct KickTarget(*mut kvm_run);

impl KickTarget {
    pub fn new(run: &mut kvm_run) -> Self {
        Self(run)
    }

    pub fn install(&self) {
        KVM_RUN.with(|cell| cell.set(self.0));
    }
}

impl Drop for KickTarget {
    fn drop(&mut self) {
        KVM_RUN.with(|cell| cell.set(null_mut()));
    }
}

fn signal_set(signals: &[c_int]) -> libc::sigset_t {
    // SAFETY: sigemptyset/sigaddset initialize a stack-allocated sigset_t.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for &signal in signals {
            libc::sigaddset(&mut set, signal);
        }
        set
    }
}

pub fn block_signals(signals: &[c_int]) -> io::Result<()> {
    let set = signal_set(signals);
    // SAFETY: the set is initialized and the old mask is not requested.
    let ret = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &set, null_mut()) };
    if ret != 0 {
        return Err(io::Error::from_raw_os_error(ret));
    }
    Ok(())
}

pub struct SignalFd(OwnedFd);

impl SignalFd {
    pub fn new(signals: &[c_int]) -> io::Result<Self> {
        let set = signal_set(signals);
        // SAFETY: the set is initialized; the returned descriptor is checked.
        let fd = unsafe { libc::signalfd(-1, &set, libc::SFD_NONBLOCK | libc::SFD_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd is a freshly created descriptor owned by nobody else.
        Ok(Self(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    pub fn read_signal(&self) -> io::Result<Option<u32>> {
        let mut info = [0u8; size_of::<libc::signalfd_siginfo>()];
        // SAFETY: the buffer is large enough for one signalfd_siginfo record.
        let ret = unsafe { libc::read(self.0.as_raw_fd(), info.as_mut_ptr().cast(), info.len()) };
        if ret < 0 {
            let err = io::Error::last_os_error();
            return if err.kind() == io::ErrorKind::WouldBlock {
                Ok(None)
            } else {
                Err(err)
            };
        }
        Ok(Some(u32::from_ne_bytes([
            info[0], info[1], info[2], info[3],
        ])))
    }
}

impl AsRawFd for SignalFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};

    use super::*;

    #[test]
    fn unwinding_out_of_the_vcpu_loop_uninstalls_the_kick_target() {
        register_kick_handler().unwrap();
        std::thread::spawn(|| {
            let mut run = Box::new(kvm_run::default());
            let target_run: *mut kvm_run = &mut *run;
            let _ = catch_unwind(AssertUnwindSafe(|| {
                // SAFETY: the box outlives this closure and nothing else accesses it meanwhile.
                let target = KickTarget::new(unsafe { &mut *target_run });
                target.install();
                resume_unwind(Box::new("vCPU loop failed"));
            }));
            // SAFETY: sending the kick signal to the current thread.
            unsafe { libc::pthread_kill(libc::pthread_self(), kick_signal()) };
            assert_eq!(run.immediate_exit, 0, "kick reached a stale kvm_run");
        })
        .join()
        .unwrap();
    }
}
