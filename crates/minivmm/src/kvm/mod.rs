pub mod irq;
pub mod vcpu;
pub mod vm;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum KvmError {
    #[error("{op} failed: {source}")]
    Ioctl {
        op: &'static str,
        source: kvm_ioctls::Error,
    },
    #[error("too many interrupt routes")]
    GsiExhausted,
    #[error("{0}")]
    Unsupported(String),
}

pub type Result<T> = std::result::Result<T, KvmError>;

pub(crate) trait IoctlContext<T> {
    fn ctx(self, op: &'static str) -> Result<T>;
}

impl<T> IoctlContext<T> for std::result::Result<T, kvm_ioctls::Error> {
    fn ctx(self, op: &'static str) -> Result<T> {
        self.map_err(|source| KvmError::Ioctl { op, source })
    }
}
