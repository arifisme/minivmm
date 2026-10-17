use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use mv_proto::agent::{FunctionError, LogStream, MAX_LOG_LINE, MAX_RESPONSE_LEN};
use mv_proto::runtime::{InvokeLine, RuntimeLine};
use serde_json::value::RawValue;

use crate::logs::LogSink;
use crate::system::{Reaper, describe_status};

const EXIT_GRACE: Duration = Duration::from_millis(500);

pub struct Launch {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub dir: PathBuf,
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum LineRead {
    Line(Vec<u8>),
    TooLong,
    Eof,
}

pub struct LineSplitter {
    buffer: Vec<u8>,
    limit: usize,
    discarding: bool,
}

impl LineSplitter {
    pub fn new(limit: usize) -> Self {
        Self {
            buffer: Vec::new(),
            limit,
            discarding: false,
        }
    }

    pub fn push(&mut self, mut data: &[u8], lines: &mut Vec<LineRead>) {
        while !data.is_empty() {
            let newline = data.iter().position(|b| *b == b'\n');
            let chunk = &data[..newline.unwrap_or(data.len())];
            if !self.discarding {
                if self.buffer.len() + chunk.len() > self.limit {
                    self.discarding = true;
                    self.buffer = Vec::new();
                } else {
                    self.buffer.extend_from_slice(chunk);
                }
            }
            let Some(end) = newline else {
                return;
            };
            lines.push(if std::mem::take(&mut self.discarding) {
                LineRead::TooLong
            } else {
                LineRead::Line(std::mem::take(&mut self.buffer))
            });
            data = &data[end + 1..];
        }
    }

    pub fn finish(&mut self, lines: &mut Vec<LineRead>) {
        if std::mem::take(&mut self.discarding) {
            lines.push(LineRead::TooLong);
        } else if !self.buffer.is_empty() {
            lines.push(LineRead::Line(std::mem::take(&mut self.buffer)));
        }
    }
}

enum Drained {
    Open,
    Closed,
}

fn drain<R: Read>(
    source: &mut R,
    splitter: &mut LineSplitter,
    lines: &mut Vec<LineRead>,
) -> Drained {
    let mut chunk = [0u8; 64 * 1024];
    loop {
        match source.read(&mut chunk) {
            Ok(0) => {
                splitter.finish(lines);
                return Drained::Closed;
            }
            Ok(n) => splitter.push(&chunk[..n], lines),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => return Drained::Open,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => {
                splitter.finish(lines);
                return Drained::Closed;
            }
        }
    }
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: querying and updating the flags of a descriptor this process owns.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    // SAFETY: as above.
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn log_lines(logs: &LogSink, lines: Vec<LineRead>) {
    for line in lines {
        match line {
            LineRead::Line(bytes) => logs.log(LogStream::Stderr, &String::from_utf8_lossy(&bytes)),
            LineRead::TooLong => logs.log(LogStream::Agent, "dropped a log line that was too long"),
            LineRead::Eof => {}
        }
    }
}

/// Reads the function's stdout and stderr on one thread. Before handing over anything read
/// from stdout it drains stderr, so log lines written before a response are delivered first.
fn pump(
    mut stdout: ChildStdout,
    mut stderr: ChildStderr,
    lines: Sender<LineRead>,
    logs: Arc<LogSink>,
) {
    if set_nonblocking(stdout.as_raw_fd()).is_err() || set_nonblocking(stderr.as_raw_fd()).is_err()
    {
        let _ = lines.send(LineRead::Eof);
        return;
    }
    let mut out = LineSplitter::new(MAX_RESPONSE_LEN + 64);
    let mut err = LineSplitter::new(MAX_LOG_LINE);
    let mut stderr_open = true;
    loop {
        let mut fds = [
            libc::pollfd {
                fd: stdout.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: if stderr_open { stderr.as_raw_fd() } else { -1 },
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: fds is a valid array of two pollfd structures.
        if unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) } < 0
            && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted
        {
            break;
        }
        if stderr_open {
            let mut logged = Vec::new();
            stderr_open = matches!(drain(&mut stderr, &mut err, &mut logged), Drained::Open);
            log_lines(&logs, logged);
        }
        let mut answered = Vec::new();
        let stdout_state = drain(&mut stdout, &mut out, &mut answered);
        for line in answered {
            if lines.send(line).is_err() {
                return;
            }
        }
        if matches!(stdout_state, Drained::Closed) {
            break;
        }
    }
    if stderr_open {
        let mut logged = Vec::new();
        drain(&mut stderr, &mut err, &mut logged);
        log_lines(&logs, logged);
    }
    let _ = lines.send(LineRead::Eof);
}

#[derive(Debug)]
pub enum InvokeOutcome {
    Response(Vec<u8>),
    FunctionError(FunctionError),
}

#[derive(Debug)]
pub enum RuntimeFailure {
    Exited(String),
    InitError(FunctionError),
    TooLarge,
    Timeout,
    Io(io::Error),
}

impl std::fmt::Display for RuntimeFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RuntimeFailure::Exited(status) => write!(f, "the function process {status}"),
            RuntimeFailure::InitError(error) => write!(f, "{}: {}", error.kind, error.message),
            RuntimeFailure::TooLarge => write!(
                f,
                "the function response exceeds {} MiB",
                MAX_RESPONSE_LEN >> 20
            ),
            RuntimeFailure::Timeout => write!(f, "the function did not answer in time"),
            RuntimeFailure::Io(err) => write!(f, "cannot talk to the function process: {err}"),
        }
    }
}

pub struct RuntimeProcess {
    pid: u32,
    stdin: ChildStdin,
    lines: Receiver<LineRead>,
    reaper: Arc<Reaper>,
    logs: Arc<LogSink>,
    exited: bool,
}

impl RuntimeProcess {
    pub fn spawn(launch: &Launch, logs: Arc<LogSink>, reaper: Arc<Reaper>) -> io::Result<Self> {
        let mut child = Command::new(&launch.program)
            .args(&launch.args)
            .current_dir(&launch.dir)
            .env_clear()
            .envs(&launch.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()?;
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err(io::Error::other("function pipes were not created"));
        };
        let pid = child.id();
        reaper.watch(child);
        let (sender, lines) = mpsc::channel();
        let pump_logs = logs.clone();
        thread::spawn(move || pump(stdout, stderr, sender, pump_logs));
        Ok(Self {
            pid,
            stdin,
            lines,
            reaper,
            logs,
            exited: false,
        })
    }

    pub fn is_running(&self) -> bool {
        !self.exited
    }

    fn exit_reason(&mut self) -> String {
        self.exited = true;
        match self.reaper.wait(self.pid, EXIT_GRACE) {
            Some(status) => describe_status(status),
            None => "closed its output".to_string(),
        }
    }

    fn next_line(&mut self, deadline: Instant) -> Result<RuntimeLine, RuntimeFailure> {
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let line = match self.lines.recv_timeout(remaining) {
                Ok(LineRead::Line(line)) => line,
                Ok(LineRead::TooLong) => return Err(RuntimeFailure::TooLarge),
                Ok(LineRead::Eof) | Err(RecvTimeoutError::Disconnected) => {
                    return Err(RuntimeFailure::Exited(self.exit_reason()));
                }
                Err(RecvTimeoutError::Timeout) => return Err(RuntimeFailure::Timeout),
            };
            match RuntimeLine::parse(&line) {
                Some(parsed) => return Ok(parsed),
                None => self
                    .logs
                    .log(LogStream::Stdout, &String::from_utf8_lossy(&line)),
            }
        }
    }

    pub fn wait_ready(&mut self, timeout: Duration) -> Result<(), RuntimeFailure> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.next_line(deadline)? {
                RuntimeLine::Ready { ready: true } => return Ok(()),
                RuntimeLine::Error { error } => return Err(RuntimeFailure::InitError(error)),
                _ => self.logs.log(
                    LogStream::Agent,
                    "ignoring a protocol line before the function was ready",
                ),
            }
        }
    }

    pub fn invoke(
        &mut self,
        request_id: &str,
        deadline_ms: u64,
        reseed: bool,
        event: &RawValue,
        timeout: Duration,
    ) -> Result<InvokeOutcome, RuntimeFailure> {
        let deadline = Instant::now() + timeout;
        let mut line = serde_json::to_vec(&InvokeLine {
            request_id,
            deadline_ms,
            reseed,
            event,
        })
        .map_err(|e| RuntimeFailure::Io(io::Error::other(e)))?;
        line.push(b'\n');
        if let Err(err) = self
            .stdin
            .write_all(&line)
            .and_then(|()| self.stdin.flush())
        {
            return Err(match err.kind() {
                io::ErrorKind::BrokenPipe => RuntimeFailure::Exited(self.exit_reason()),
                _ => RuntimeFailure::Io(err),
            });
        }
        loop {
            match self.next_line(deadline)? {
                RuntimeLine::Response { response } => {
                    return Ok(InvokeOutcome::Response(response.get().as_bytes().to_vec()));
                }
                RuntimeLine::Error { error } => return Ok(InvokeOutcome::FunctionError(error)),
                RuntimeLine::Ready { .. } => {}
            }
        }
    }

    pub fn kill(&mut self) {
        if !self.exited {
            // SAFETY: signalling the process group created for the function.
            unsafe { libc::kill(-(self.pid as i32), libc::SIGKILL) };
            self.exit_reason();
        }
    }
}

impl Drop for RuntimeProcess {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(chunks: &[&[u8]], limit: usize) -> Vec<LineRead> {
        let mut splitter = LineSplitter::new(limit);
        let mut lines = Vec::new();
        for chunk in chunks {
            splitter.push(chunk, &mut lines);
        }
        splitter.finish(&mut lines);
        lines
    }

    #[test]
    fn splitter_bounds_lines_across_chunks() {
        assert_eq!(
            split(
                &[b"sho", b"rt\nthis one is far ", b"too long\nok\nta", b"il"],
                8
            ),
            vec![
                LineRead::Line(b"short".to_vec()),
                LineRead::TooLong,
                LineRead::Line(b"ok".to_vec()),
                LineRead::Line(b"tail".to_vec()),
            ]
        );
        assert!(split(&[b""], 8).is_empty());
        assert_eq!(split(&[b"\n"], 8), vec![LineRead::Line(Vec::new())]);
        assert_eq!(split(&[b"0123456789"], 8), vec![LineRead::TooLong]);
        assert_eq!(
            split(&[b"01234567", b"\n"], 8),
            vec![LineRead::Line(b"01234567".to_vec())]
        );
    }
}
