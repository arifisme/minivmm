use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;

pub const VNET_HEADER_LEN: usize = 12;
const IFNAMSIZ: usize = 16;
const TUN_DEVICE: &str = "/dev/net/tun";

#[repr(C)]
struct InterfaceRequest {
    name: [u8; IFNAMSIZ],
    flags: libc::c_short,
    padding: [u8; 22],
}

pub struct Tap {
    file: File,
    name: String,
}

fn ioctl_with_ptr<T>(fd: RawFd, request: libc::Ioctl, arg: *const T) -> io::Result<()> {
    // SAFETY: callers pass a request whose argument layout matches `T`.
    let ret = unsafe { libc::ioctl(fd, request, arg) };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

impl Tap {
    pub fn open(name: &str) -> io::Result<Self> {
        if name.is_empty() || name.len() >= IFNAMSIZ {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "interface name must be 1-15 bytes",
            ));
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(TUN_DEVICE)?;
        let mut request = InterfaceRequest {
            name: [0; IFNAMSIZ],
            flags: (libc::IFF_TAP | libc::IFF_NO_PI | libc::IFF_VNET_HDR) as libc::c_short,
            padding: [0; 22],
        };
        request.name[..name.len()].copy_from_slice(name.as_bytes());
        let fd = file.as_raw_fd();
        ioctl_with_ptr(fd, libc::TUNSETIFF, &request)?;
        let header_len = VNET_HEADER_LEN as libc::c_int;
        ioctl_with_ptr(fd, libc::TUNSETVNETHDRSZ, &header_len)?;
        // SAFETY: TUNSETOFFLOAD takes its flags by value.
        if unsafe { libc::ioctl(fd, libc::TUNSETOFFLOAD, 0 as libc::c_ulong) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            file,
            name: name.to_string(),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn read_frame(&self, buf: &mut [u8]) -> io::Result<usize> {
        (&self.file).read(buf)
    }

    pub fn write_frame(&self, frame: &[u8]) -> io::Result<usize> {
        (&self.file).write(frame)
    }
}

impl AsRawFd for Tap {
    fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}
