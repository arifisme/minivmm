use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use crate::json::Json;

const MAX_REQUEST_LEN: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApiCommand {
    Status,
    Pause,
    Resume,
    Snapshot(PathBuf),
    Shutdown,
}

pub fn parse_command(line: &str) -> Result<ApiCommand, String> {
    let request = Json::parse(line).map_err(|err| err.to_string())?;
    let command = request
        .get("cmd")
        .and_then(Json::as_str)
        .ok_or("request needs a string \"cmd\" field")?;
    match command {
        "status" => Ok(ApiCommand::Status),
        "pause" => Ok(ApiCommand::Pause),
        "resume" => Ok(ApiCommand::Resume),
        "shutdown" => Ok(ApiCommand::Shutdown),
        "snapshot" => request
            .get("path")
            .and_then(Json::as_str)
            .filter(|path| !path.is_empty())
            .map(|path| ApiCommand::Snapshot(PathBuf::from(path)))
            .ok_or_else(|| "snapshot needs a string \"path\" field".to_string()),
        other => Err(format!("unknown command {other:?}")),
    }
}

pub fn error_response(message: &str) -> Json {
    Json::object([("ok", Json::from(false)), ("error", Json::from(message))])
}

struct Client {
    stream: UnixStream,
    buffer: Vec<u8>,
}

pub struct ApiServer {
    listener: UnixListener,
    path: PathBuf,
    clients: BTreeMap<u32, Client>,
    next_id: u32,
}

pub struct ClientInput {
    pub requests: Vec<String>,
    pub closed: bool,
}

impl ApiServer {
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
            path: path.to_path_buf(),
            clients: BTreeMap::new(),
            next_id: 0,
        })
    }

    pub fn listener_fd(&self) -> RawFd {
        self.listener.as_raw_fd()
    }

    pub fn accept(&mut self) -> Vec<(u32, RawFd)> {
        let mut accepted = Vec::new();
        while let Ok((stream, _)) = self.listener.accept() {
            if stream.set_nonblocking(true).is_err() {
                continue;
            }
            let id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1);
            accepted.push((id, stream.as_raw_fd()));
            self.clients.insert(
                id,
                Client {
                    stream,
                    buffer: Vec::new(),
                },
            );
        }
        accepted
    }

    pub fn read(&mut self, id: u32) -> ClientInput {
        let Some(client) = self.clients.get_mut(&id) else {
            return ClientInput {
                requests: Vec::new(),
                closed: true,
            };
        };
        let mut closed = false;
        let mut chunk = [0u8; 4096];
        loop {
            match client.stream.read(&mut chunk) {
                Ok(0) => {
                    closed = true;
                    break;
                }
                Ok(n) => client.buffer.extend_from_slice(&chunk[..n]),
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    closed = true;
                    break;
                }
            }
        }
        let mut requests = Vec::new();
        while let Some(end) = client.buffer.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = client.buffer.drain(..=end).collect();
            let text = String::from_utf8_lossy(&line).trim().to_string();
            if !text.is_empty() {
                requests.push(text);
            }
        }
        if client.buffer.len() > MAX_REQUEST_LEN {
            closed = true;
        }
        ClientInput { requests, closed }
    }

    pub fn respond(&mut self, id: u32, response: &Json) {
        if let Some(client) = self.clients.get_mut(&id) {
            let line = format!("{response}\n");
            let _ = client.stream.set_nonblocking(false);
            let _ = client.stream.write_all(line.as_bytes());
            let _ = client.stream.set_nonblocking(true);
        }
    }

    pub fn client_fd(&self, id: u32) -> Option<RawFd> {
        self.clients.get(&id).map(|c| c.stream.as_raw_fd())
    }

    pub fn remove(&mut self, id: u32) {
        self.clients.remove(&id);
    }
}

impl Drop for ApiServer {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader};

    use super::*;

    #[test]
    fn parses_known_commands() {
        assert_eq!(parse_command(r#"{"cmd":"status"}"#), Ok(ApiCommand::Status));
        assert_eq!(parse_command(r#"{"cmd":"pause"}"#), Ok(ApiCommand::Pause));
        assert_eq!(parse_command(r#"{"cmd":"resume"}"#), Ok(ApiCommand::Resume));
        assert_eq!(
            parse_command(r#"{"cmd":"shutdown"}"#),
            Ok(ApiCommand::Shutdown)
        );
        assert_eq!(
            parse_command(r#"{"cmd":"snapshot","path":"/tmp/vm.snap"}"#),
            Ok(ApiCommand::Snapshot(PathBuf::from("/tmp/vm.snap")))
        );
    }

    #[test]
    fn rejects_bad_requests() {
        assert!(parse_command("status").is_err());
        assert!(parse_command(r#"{"command":"status"}"#).is_err());
        assert!(parse_command(r#"{"cmd":"reboot"}"#).is_err());
        assert!(parse_command(r#"{"cmd":"snapshot"}"#).is_err());
        assert!(parse_command(r#"{"cmd":"snapshot","path":""}"#).is_err());
        assert!(parse_command(r#"{"cmd":5}"#).is_err());
    }

    #[test]
    fn serves_line_requests() {
        let dir = vmm_sys_util::tempdir::TempDir::new().unwrap();
        let path = dir.as_path().join("api.sock");
        let mut server = ApiServer::bind(&path).unwrap();
        let mut client = UnixStream::connect(&path).unwrap();
        let accepted = server.accept();
        assert_eq!(accepted.len(), 1);
        let id = accepted[0].0;
        client
            .write_all(b"{\"cmd\":\"status\"}\n{\"cmd\":\"pa")
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let input = server.read(id);
        assert_eq!(input.requests, vec![r#"{"cmd":"status"}"#.to_string()]);
        assert!(!input.closed);
        client.write_all(b"use\"}\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(
            server.read(id).requests,
            vec![r#"{"cmd":"pause"}"#.to_string()]
        );
        server.respond(id, &error_response("nope"));
        let mut line = String::new();
        BufReader::new(&client).read_line(&mut line).unwrap();
        assert_eq!(line, "{\"error\":\"nope\",\"ok\":false}\n");
        drop(client);
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(server.read(id).closed);
        drop(server);
        assert!(!path.exists());
    }
}
