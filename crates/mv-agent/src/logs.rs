use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::{Arc, Mutex, MutexGuard};

use mv_proto::agent::{LogStream, MAX_LOG_LINE, Reply};
use mv_proto::frame;

const MAX_BACKLOG: usize = 1024;

#[derive(Default)]
struct State {
    writer: Option<Box<dyn Write + Send>>,
    backlog: VecDeque<Vec<u8>>,
    request_id: Option<String>,
}

/// Owns the connection's write side so that replies and log lines from the function's output
/// threads never interleave inside a frame.
#[derive(Default)]
pub struct LogSink {
    state: Mutex<State>,
}

fn truncate(line: &str) -> &str {
    if line.len() <= MAX_LOG_LINE {
        return line;
    }
    let mut end = MAX_LOG_LINE;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    &line[..end]
}

impl LogSink {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn attach(&self, writer: Box<dyn Write + Send>) {
        let mut state = self.lock();
        state.writer = Some(writer);
        while let Some(frame) = state.backlog.pop_front() {
            if !Self::write(&mut state, &frame) {
                break;
            }
        }
    }

    pub fn detach(&self) {
        self.lock().writer = None;
    }

    pub fn set_request(&self, request_id: Option<String>) {
        self.lock().request_id = request_id;
    }

    fn write(state: &mut State, frame: &[u8]) -> bool {
        let Some(writer) = &mut state.writer else {
            return false;
        };
        let written = writer.write_all(frame).and_then(|()| writer.flush());
        if written.is_err() {
            state.writer = None;
        }
        written.is_ok()
    }

    pub fn log(&self, stream: LogStream, line: &str) {
        let mut state = self.lock();
        let reply = Reply::Log {
            request_id: state.request_id.clone(),
            stream,
            line: truncate(line.trim_end_matches('\r')).to_string(),
        };
        let Ok(frame) = frame::encode(&reply, &[]) else {
            return;
        };
        if !Self::write(&mut state, &frame) {
            if state.backlog.len() == MAX_BACKLOG {
                state.backlog.pop_front();
            }
            state.backlog.push_back(frame);
        }
    }

    pub fn send(&self, reply: &Reply, payload: &[u8]) -> io::Result<()> {
        let frame = frame::encode(reply, payload).map_err(io::Error::other)?;
        if Self::write(&mut self.lock(), &frame) {
            Ok(())
        } else {
            Err(io::ErrorKind::BrokenPipe.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn replies(wire: &[u8]) -> Vec<Reply> {
        let mut reader = wire;
        let mut out = Vec::new();
        while !reader.is_empty() {
            out.push(frame::read_frame::<_, Reply>(&mut reader).unwrap().0);
        }
        out
    }

    #[test]
    fn logs_are_tagged_buffered_and_flushed_on_attach() {
        let sink = LogSink::new();
        sink.log(LogStream::Stderr, "before\r");
        let wire = Shared::default();
        sink.attach(Box::new(wire.clone()));
        sink.set_request(Some("r1".into()));
        sink.log(LogStream::Stdout, &"é".repeat(MAX_LOG_LINE));
        sink.send(&Reply::Synced { applied: false }, &[]).unwrap();
        let got = replies(&wire.0.lock().unwrap());
        assert_eq!(got.len(), 3);
        assert_eq!(
            got[0],
            Reply::Log {
                request_id: None,
                stream: LogStream::Stderr,
                line: "before".into()
            }
        );
        match &got[1] {
            Reply::Log {
                request_id, line, ..
            } => {
                assert_eq!(request_id.as_deref(), Some("r1"));
                assert!(line.len() <= MAX_LOG_LINE);
            }
            other => panic!("unexpected {other:?}"),
        }
        sink.detach();
        assert!(sink.send(&Reply::Synced { applied: false }, &[]).is_err());
        for _ in 0..MAX_BACKLOG + 10 {
            sink.log(LogStream::Agent, "x");
        }
        assert_eq!(sink.lock().backlog.len(), MAX_BACKLOG);
    }
}
