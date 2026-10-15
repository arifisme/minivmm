use std::io;
use std::sync::{Arc, Mutex};

use log::warn;
use vmm_sys_util::eventfd::{EFD_NONBLOCK, EventFd};

use crate::sync::LockExt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitReason {
    Reboot,
    PowerOff,
    HostRequest,
    Fault,
}

pub struct ExitSignal {
    event: EventFd,
    reason: Mutex<Option<ExitReason>>,
}

impl ExitSignal {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            event: EventFd::new(EFD_NONBLOCK)?,
            reason: Mutex::new(None),
        })
    }

    pub fn request(&self, reason: ExitReason) {
        self.reason.lock_or_recover().get_or_insert(reason);
        if let Err(err) = self.event.write(1) {
            warn!("failed to signal VM exit: {err}");
        }
    }

    pub fn reason(&self) -> Option<ExitReason> {
        *self.reason.lock_or_recover()
    }

    pub fn event(&self) -> &EventFd {
        &self.event
    }
}

pub struct ExitOnPanic(Arc<ExitSignal>);

impl ExitOnPanic {
    pub fn new(signal: Arc<ExitSignal>) -> Self {
        Self(signal)
    }
}

impl Drop for ExitOnPanic {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.request(ExitReason::Fault);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_reason_wins() {
        let signal = ExitSignal::new().unwrap();
        assert_eq!(signal.reason(), None);
        signal.request(ExitReason::Reboot);
        signal.request(ExitReason::PowerOff);
        assert_eq!(signal.reason(), Some(ExitReason::Reboot));
        assert_eq!(signal.event().read().unwrap(), 2);
    }

    #[test]
    fn a_panicking_thread_requests_a_fault_exit() {
        let signal = Arc::new(ExitSignal::new().unwrap());
        let guard = ExitOnPanic::new(signal.clone());
        let result = std::thread::spawn(move || {
            let _guard = guard;
            std::panic::resume_unwind(Box::new("vCPU loop failed"));
        })
        .join();
        assert!(result.is_err());
        assert_eq!(signal.reason(), Some(ExitReason::Fault));

        let quiet = Arc::new(ExitSignal::new().unwrap());
        drop(ExitOnPanic::new(quiet.clone()));
        assert_eq!(quiet.reason(), None);
    }
}
