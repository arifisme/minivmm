pub mod acpi_pm;
pub mod bus;
pub mod i8042;
pub mod pci;
pub mod serial;

use log::warn;
use vmm_sys_util::eventfd::EventFd;

pub trait Interrupt: Send {
    fn trigger(&self);
}

impl Interrupt for EventFd {
    fn trigger(&self) {
        if let Err(err) = self.write(1) {
            warn!("failed to inject interrupt: {err}");
        }
    }
}

fn read_register_bytes(register: &[u8], offset: u64, data: &mut [u8]) {
    for (i, byte) in data.iter_mut().enumerate() {
        *byte = usize::try_from(offset)
            .ok()
            .and_then(|o| o.checked_add(i))
            .and_then(|o| register.get(o))
            .copied()
            .unwrap_or(0);
    }
}

#[cfg(test)]
pub(crate) mod test_utils {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::Interrupt;

    #[derive(Clone, Default)]
    pub struct CountingInterrupt(pub Arc<AtomicUsize>);

    impl CountingInterrupt {
        pub fn count(&self) -> usize {
            self.0.load(Ordering::SeqCst)
        }
    }

    impl Interrupt for CountingInterrupt {
        fn trigger(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
}
