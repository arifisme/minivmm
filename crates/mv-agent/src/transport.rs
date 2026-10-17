use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixListener;
use std::path::Path;

pub type Connection = (Box<dyn Read + Send>, Box<dyn Write + Send>);

pub enum Listener {
    Vsock(OwnedFd),
    Unix(UnixListener),
}

fn check(result: libc::c_int) -> io::Result<libc::c_int> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

impl Listener {
    pub fn vsock(port: u32) -> io::Result<Self> {
        // SAFETY: creating a socket; the descriptor is checked and owned below.
        let fd = check(unsafe {
            libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0)
        })?;
        // SAFETY: fd is a fresh descriptor that nothing else owns.
        let socket = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: sockaddr_vm is plain data, so all-zero is a valid value.
        let mut address: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
        address.svm_family = libc::AF_VSOCK as libc::sa_family_t;
        address.svm_port = port;
        address.svm_cid = libc::VMADDR_CID_ANY;
        // SAFETY: address is a valid sockaddr_vm of the given length.
        check(unsafe {
            libc::bind(
                socket.as_raw_fd(),
                (&address as *const libc::sockaddr_vm).cast(),
                size_of::<libc::sockaddr_vm>() as libc::socklen_t,
            )
        })?;
        // SAFETY: listening on a bound socket.
        check(unsafe { libc::listen(socket.as_raw_fd(), 16) })?;
        Ok(Listener::Vsock(socket))
    }

    pub fn unix(path: &Path) -> io::Result<Self> {
        let _ = std::fs::remove_file(path);
        Ok(Listener::Unix(UnixListener::bind(path)?))
    }

    pub fn accept(&self) -> io::Result<Connection> {
        match self {
            Listener::Vsock(socket) => {
                // SAFETY: accepting on a listening socket without retrieving the peer address.
                let fd = check(unsafe {
                    libc::accept4(
                        socket.as_raw_fd(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        libc::SOCK_CLOEXEC,
                    )
                })?;
                // SAFETY: fd is a fresh descriptor that nothing else owns.
                let stream = File::from(unsafe { OwnedFd::from_raw_fd(fd) });
                Ok((Box::new(stream.try_clone()?), Box::new(stream)))
            }
            Listener::Unix(listener) => {
                let (stream, _) = listener.accept()?;
                Ok((Box::new(stream.try_clone()?), Box::new(stream)))
            }
        }
    }
}
