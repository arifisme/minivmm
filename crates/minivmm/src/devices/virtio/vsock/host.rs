use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use super::VsockBackend;
use super::connection::VsockStream;

const MAX_REQUEST_LINE: usize = 64;
const MAX_HANDSHAKES: usize = 16;

impl VsockStream for UnixStream {
    fn raw_fd(&self) -> Option<RawFd> {
        Some(self.as_raw_fd())
    }

    fn shutdown_write(&mut self) -> io::Result<()> {
        self.shutdown(Shutdown::Write)
    }
}

/// Connects guest-initiated connections to port `P` to the Unix socket `<path>_<P>`.
pub struct UnixBackend {
    path: PathBuf,
}

impl UnixBackend {
    pub fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
        }
    }
}

impl VsockBackend for UnixBackend {
    fn connect(&mut self, port: u32) -> io::Result<Box<dyn VsockStream>> {
        let mut target = self.path.as_os_str().to_owned();
        target.push(format!("_{port}"));
        let stream = UnixStream::connect(target)?;
        stream.set_nonblocking(true)?;
        Ok(Box::new(stream))
    }
}

pub enum Handshake {
    Pending,
    Failed,
    Connect {
        stream: UnixStream,
        port: u32,
        early_data: Vec<u8>,
    },
}

struct PendingHandshake {
    stream: UnixStream,
    buffer: Vec<u8>,
}

/// Accepts host connections on the vsock Unix socket and reads their `CONNECT <port>` line.
pub struct HostListener {
    listener: UnixListener,
    pending: BTreeMap<u32, PendingHandshake>,
    next_id: u32,
}

pub fn parse_connect(line: &[u8]) -> Option<u32> {
    let text = std::str::from_utf8(line).ok()?;
    let port = text.strip_prefix("CONNECT ")?.trim_end_matches('\r');
    if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    port.parse().ok()
}

impl HostListener {
    pub fn bind(path: &Path) -> io::Result<Self> {
        if let Ok(metadata) = fs::symlink_metadata(path)
            && metadata.file_type().is_socket()
        {
            fs::remove_file(path)?;
        }
        let listener = UnixListener::bind(path)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            pending: BTreeMap::new(),
            next_id: 0,
        })
    }

    pub fn listener_fd(&self) -> RawFd {
        self.listener.as_raw_fd()
    }

    pub fn accept(&mut self) -> Vec<(u32, RawFd)> {
        let mut accepted = Vec::new();
        while let Ok((stream, _)) = self.listener.accept() {
            if self.pending.len() >= MAX_HANDSHAKES || stream.set_nonblocking(true).is_err() {
                continue;
            }
            while self.pending.contains_key(&self.next_id) {
                self.next_id = self.next_id.wrapping_add(1);
            }
            let id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1);
            accepted.push((id, stream.as_raw_fd()));
            self.pending.insert(
                id,
                PendingHandshake {
                    stream,
                    buffer: Vec::new(),
                },
            );
        }
        accepted
    }

    pub fn handshake_fd(&self, id: u32) -> Option<RawFd> {
        self.pending.get(&id).map(|p| p.stream.as_raw_fd())
    }

    pub fn read(&mut self, id: u32) -> Handshake {
        let Some(pending) = self.pending.get_mut(&id) else {
            return Handshake::Failed;
        };
        let mut chunk = [0u8; MAX_REQUEST_LINE];
        let room = MAX_REQUEST_LINE - pending.buffer.len();
        match pending.stream.read(&mut chunk[..room]) {
            Ok(0) => {
                self.pending.remove(&id);
                return Handshake::Failed;
            }
            Ok(n) => pending.buffer.extend_from_slice(&chunk[..n]),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => return Handshake::Pending,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => return Handshake::Pending,
            Err(_) => {
                self.pending.remove(&id);
                return Handshake::Failed;
            }
        }
        let Some(newline) = pending.buffer.iter().position(|b| *b == b'\n') else {
            if pending.buffer.len() >= MAX_REQUEST_LINE {
                self.pending.remove(&id);
                return Handshake::Failed;
            }
            return Handshake::Pending;
        };
        let Some(pending) = self.pending.remove(&id) else {
            return Handshake::Failed;
        };
        match parse_connect(&pending.buffer[..newline]) {
            Some(port) => Handshake::Connect {
                stream: pending.stream,
                port,
                early_data: pending.buffer[newline + 1..].to_vec(),
            },
            None => Handshake::Failed,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use vmm_sys_util::tempdir::TempDir;

    use super::*;

    #[test]
    fn parses_connect_lines() {
        assert_eq!(parse_connect(b"CONNECT 52"), Some(52));
        assert_eq!(parse_connect(b"CONNECT 4294967295\r"), Some(u32::MAX));
        assert_eq!(parse_connect(b"CONNECT 4294967296"), None);
        assert_eq!(parse_connect(b"CONNECT "), None);
        assert_eq!(parse_connect(b"CONNECT -1"), None);
        assert_eq!(parse_connect(b"connect 52"), None);
        assert_eq!(parse_connect(b"CONNECT +5"), None);
        assert_eq!(parse_connect(&[0xff, 0xfe]), None);
    }

    #[test]
    fn reads_the_request_line_and_keeps_early_data() {
        let dir = TempDir::new().unwrap();
        let path = dir.as_path().join("v.sock");
        let mut listener = HostListener::bind(&path).unwrap();
        let mut client = UnixStream::connect(&path).unwrap();
        let accepted = listener.accept();
        assert_eq!(accepted.len(), 1);
        let id = accepted[0].0;
        assert!(matches!(listener.read(id), Handshake::Pending));
        client.write_all(b"CONNECT 10").unwrap();
        assert!(matches!(listener.read(id), Handshake::Pending));
        client.write_all(b"24\nhello").unwrap();
        match listener.read(id) {
            Handshake::Connect {
                port, early_data, ..
            } => {
                assert_eq!(port, 1024);
                assert_eq!(early_data, b"hello");
            }
            _ => panic!("handshake did not complete"),
        }
        assert!(listener.handshake_fd(id).is_none());
    }

    #[test]
    fn rejects_garbage_and_overlong_requests() {
        let dir = TempDir::new().unwrap();
        let path = dir.as_path().join("v.sock");
        let mut listener = HostListener::bind(&path).unwrap();
        let mut bad = UnixStream::connect(&path).unwrap();
        let mut long = UnixStream::connect(&path).unwrap();
        let ids: Vec<u32> = listener.accept().into_iter().map(|(id, _)| id).collect();
        bad.write_all(b"HELLO\n").unwrap();
        long.write_all(&[b'1'; 100]).unwrap();
        assert!(matches!(listener.read(ids[0]), Handshake::Failed));
        assert!(matches!(listener.read(ids[1]), Handshake::Failed));
        drop(UnixStream::connect(&path).unwrap());
        let closed = listener.accept()[0].0;
        assert!(matches!(listener.read(closed), Handshake::Failed));
    }

    #[test]
    fn guest_connections_reach_port_suffixed_sockets() {
        let dir = TempDir::new().unwrap();
        let base = dir.as_path().join("v.sock");
        let target = UnixListener::bind(dir.as_path().join("v.sock_77")).unwrap();
        let mut backend = UnixBackend::new(&base);
        let mut stream = backend.connect(77).unwrap();
        let (mut peer, _) = target.accept().unwrap();
        stream.write_all(b"ping").unwrap();
        let mut buf = [0u8; 4];
        peer.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"ping");
        assert!(backend.connect(78).is_err());
    }
}
