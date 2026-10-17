use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use mv_proto::agent::{LogStream, MAX_RESPONSE_LEN, Reply, Request};
use mv_proto::frame::{self, FrameLengths, PREFIX_LEN};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

const MAX_REPLY_HEADER: usize = 64 << 10;
const MAX_REPLY_PAYLOAD: usize = MAX_RESPONSE_LEN + 4096;
const MAX_HANDSHAKE_LINE: usize = 64;
const RETRY_DELAY: Duration = Duration::from_millis(2);

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("cannot reach the agent: {0}")]
    Connect(String),
    #[error("agent connection failed: {0}")]
    Io(#[from] io::Error),
    #[error("agent sent an invalid message: {0}")]
    Protocol(String),
    #[error("the agent did not answer within {0:?}")]
    Timeout(Duration),
}

impl AgentError {
    pub fn is_disconnect(&self) -> bool {
        matches!(self, AgentError::Io(_))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogLine {
    pub request_id: Option<String>,
    pub stream: LogStream,
    pub line: String,
}

pub struct AgentClient {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl AgentClient {
    /// Connects through the VMM's vsock socket, retrying until `deadline` while the VMM is
    /// still starting or the guest has not accepted yet.
    pub async fn connect(uds: &Path, port: u32, deadline: Instant) -> Result<Self, AgentError> {
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let attempt = tokio::time::timeout(remaining, Self::try_connect(uds, port)).await;
            match attempt {
                Ok(Ok(client)) => return Ok(client),
                Ok(Err(err)) if Instant::now() + RETRY_DELAY < deadline => {
                    tracing::trace!("agent not reachable yet: {err}");
                    tokio::time::sleep(RETRY_DELAY).await;
                }
                Ok(Err(err)) => return Err(AgentError::Connect(err.to_string())),
                Err(_) => return Err(AgentError::Connect("timed out".into())),
            }
        }
    }

    async fn try_connect(uds: &Path, port: u32) -> io::Result<Self> {
        let stream = UnixStream::connect(uds).await?;
        let (read, mut writer) = stream.into_split();
        writer
            .write_all(format!("CONNECT {port}\n").as_bytes())
            .await?;
        let mut reader = BufReader::new(read);
        let mut line = Vec::new();
        (&mut reader)
            .take(MAX_HANDSHAKE_LINE as u64)
            .read_until(b'\n', &mut line)
            .await?;
        if !line.starts_with(b"OK ") || !line.ends_with(b"\n") {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                "the guest refused the connection",
            ));
        }
        Ok(Self { reader, writer })
    }

    pub async fn call(
        &mut self,
        request: &Request,
        payload: &[u8],
        timeout: Duration,
        on_log: &mut (dyn FnMut(LogLine) + Send),
    ) -> Result<(Reply, Vec<u8>), AgentError> {
        tokio::time::timeout(timeout, self.exchange(request, payload, on_log))
            .await
            .map_err(|_| AgentError::Timeout(timeout))?
    }

    async fn exchange(
        &mut self,
        request: &Request,
        payload: &[u8],
        on_log: &mut (dyn FnMut(LogLine) + Send),
    ) -> Result<(Reply, Vec<u8>), AgentError> {
        let frame =
            frame::encode(request, payload).map_err(|e| AgentError::Protocol(e.to_string()))?;
        self.writer.write_all(&frame).await?;
        loop {
            match self.read_reply().await? {
                (
                    Reply::Log {
                        request_id,
                        stream,
                        line,
                    },
                    _,
                ) => on_log(LogLine {
                    request_id,
                    stream,
                    line,
                }),
                reply => return Ok(reply),
            }
        }
    }

    async fn read_reply(&mut self) -> Result<(Reply, Vec<u8>), AgentError> {
        let mut prefix = [0u8; PREFIX_LEN];
        self.reader.read_exact(&mut prefix).await?;
        let lengths =
            frame::decode_prefix(&prefix).map_err(|e| AgentError::Protocol(e.to_string()))?;
        check_reply_lengths(lengths)?;
        let mut body = vec![0u8; lengths.total()];
        self.reader.read_exact(&mut body).await?;
        frame::decode_body(lengths, body).map_err(|e| AgentError::Protocol(e.to_string()))
    }
}

fn check_reply_lengths(lengths: FrameLengths) -> Result<(), AgentError> {
    if lengths.header > MAX_REPLY_HEADER || lengths.payload > MAX_REPLY_PAYLOAD {
        return Err(AgentError::Protocol(format!(
            "reply of {} + {} bytes is too large",
            lengths.header, lengths.payload
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use tokio::net::UnixListener;

    use super::*;

    async fn fake_vmm(path: &Path, script: Vec<Vec<u8>>, accept: bool) {
        let listener = UnixListener::bind(path).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut reader = BufReader::new(read);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            assert_eq!(line, "CONNECT 52\n");
            if !accept {
                return;
            }
            write.write_all(b"OK 1073741824\n").await.unwrap();
            let mut prefix = [0u8; PREFIX_LEN];
            reader.read_exact(&mut prefix).await.unwrap();
            let lengths = frame::decode_prefix(&prefix).unwrap();
            let mut body = vec![0u8; lengths.total()];
            reader.read_exact(&mut body).await.unwrap();
            for chunk in script {
                write.write_all(&chunk).await.unwrap();
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
    }

    #[tokio::test]
    async fn calls_collect_logs_until_the_reply() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.sock");
        let log = Reply::Log {
            request_id: Some("r".into()),
            stream: LogStream::Stderr,
            line: "hello".into(),
        };
        let reply = Reply::Synced { applied: true };
        fake_vmm(
            &path,
            vec![
                frame::encode(&log, &[]).unwrap(),
                frame::encode(&reply, b"x").unwrap(),
            ],
            true,
        )
        .await;
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut client = AgentClient::connect(&path, 52, deadline).await.unwrap();
        let mut logs = Vec::new();
        let (got, payload) = client
            .call(
                &Request::Sync { realtime_ns: 1 },
                &[0; 16],
                Duration::from_secs(5),
                &mut |line| logs.push(line),
            )
            .await
            .unwrap();
        assert_eq!(got, reply);
        assert_eq!(payload, b"x");
        assert_eq!(logs[0].line, "hello");
    }

    #[tokio::test]
    async fn oversized_replies_and_silence_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.sock");
        let mut huge = Vec::new();
        huge.extend_from_slice(&2u32.to_le_bytes());
        huge.extend_from_slice(&(MAX_REPLY_PAYLOAD as u32 + 1).to_le_bytes());
        fake_vmm(&path, vec![huge], true).await;
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut client = AgentClient::connect(&path, 52, deadline).await.unwrap();
        let result = client
            .call(&Request::Health, &[], Duration::from_secs(5), &mut |_| {})
            .await;
        assert!(matches!(result, Err(AgentError::Protocol(_))));

        let quiet = dir.path().join("quiet.sock");
        fake_vmm(&quiet, vec![], true).await;
        let mut client = AgentClient::connect(&quiet, 52, deadline).await.unwrap();
        let result = client
            .call(
                &Request::Health,
                &[],
                Duration::from_millis(100),
                &mut |_| {},
            )
            .await;
        assert!(matches!(result, Err(AgentError::Timeout(_))));
    }

    #[tokio::test]
    async fn refused_and_missing_sockets_fail_after_the_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.sock");
        fake_vmm(&path, vec![], false).await;
        let deadline = Instant::now() + Duration::from_millis(200);
        assert!(matches!(
            AgentClient::connect(&path, 52, deadline).await,
            Err(AgentError::Connect(_))
        ));
        let missing = dir.path().join("missing.sock");
        let started = Instant::now();
        assert!(
            AgentClient::connect(&missing, 52, Instant::now() + Duration::from_millis(100))
                .await
                .is_err()
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
