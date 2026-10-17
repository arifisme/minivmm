use std::collections::BTreeMap;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use flate2::Compression;
use flate2::write::GzEncoder;
use mv_proto::agent::{ErrorKind, FunctionSetup, LogStream, Reply, Request, Runtime};
use mv_proto::frame;

struct Agent {
    child: Child,
    socket: PathBuf,
    _dir: tempfile::TempDir,
}

impl Agent {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("agent.sock");
        let child = Command::new(env!("CARGO_BIN_EXE_mv-agent"))
            .arg("--listen")
            .arg(format!("unix:{}", socket.display()))
            .arg("--work-dir")
            .arg(dir.path().join("work"))
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !socket.exists() {
            assert!(Instant::now() < deadline, "the agent never listened");
            thread::sleep(Duration::from_millis(10));
        }
        Self {
            child,
            socket,
            _dir: dir,
        }
    }

    fn connect(&self) -> Client {
        let stream = UnixStream::connect(&self.socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        Client { stream }
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Client {
    stream: UnixStream,
}

struct Response {
    reply: Reply,
    payload: Vec<u8>,
    logs: Vec<(Option<String>, LogStream, String)>,
}

impl Client {
    fn call(&mut self, request: Request, payload: &[u8]) -> Response {
        frame::write_frame(&mut self.stream, &request, payload).unwrap();
        let mut logs = Vec::new();
        loop {
            let (reply, payload): (Reply, Vec<u8>) = frame::read_frame(&mut self.stream).unwrap();
            match reply {
                Reply::Log {
                    request_id,
                    stream,
                    line,
                } => logs.push((request_id, stream, line)),
                reply => {
                    return Response {
                        reply,
                        payload,
                        logs,
                    };
                }
            }
        }
    }

    fn invoke(&mut self, request_id: &str, event: &str) -> Response {
        let deadline_ms = now_ms() + 5000;
        self.call(
            Request::Invoke {
                request_id: request_id.into(),
                deadline_ms,
            },
            event.as_bytes(),
        )
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn archive(files: &[(&str, &str, u32)]) -> Vec<u8> {
    let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
    for (path, contents, mode) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(*mode);
        header.set_cksum();
        builder
            .append_data(&mut header, path, contents.as_bytes())
            .unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap()
}

fn setup(runtime: Runtime, handler: &str) -> FunctionSetup {
    FunctionSetup {
        name: "test".into(),
        runtime,
        handler: handler.into(),
        memory_mib: 128,
        env: BTreeMap::from([("GREETING".to_string(), "hello".to_string())]),
    }
}

const SHELL_FUNCTION: &str = r#"#!/bin/sh
echo "starting with $GREETING" >&2
echo "not a protocol line"
echo '{"ready":true}'
while read -r line; do
  case "$line" in
    *'"exit"'*) echo "leaving" >&2; exit 3 ;;
    *'"fail"'*) echo '{"error":{"type":"Boom","message":"asked to fail"}}' ;;
    *'"reseed":true'*) echo '{"response":{"reseeded":true}}' ;;
    *) echo "handling $MV_FUNCTION_NAME" >&2; echo "{\"response\":{\"length\":${#line}}}" ;;
  esac
done
"#;

fn ready_shell_agent(agent: &Agent) -> Client {
    let mut client = agent.connect();
    let loaded = client.call(
        Request::Load {
            function: setup(Runtime::Native, "bin/handler"),
        },
        &archive(&[("bin/handler", SHELL_FUNCTION, 0o755)]),
    );
    assert!(
        matches!(loaded.reply, Reply::Loaded { files: 1, .. }),
        "{:?}",
        loaded.reply
    );
    let init = client.call(Request::Init { timeout_ms: 5000 }, &[]);
    assert!(
        matches!(init.reply, Reply::Initialized { .. }),
        "{:?}",
        init.reply
    );
    assert!(
        init.logs
            .iter()
            .any(|(_, stream, line)| *stream == LogStream::Stderr && line == "starting with hello")
    );
    assert!(
        init.logs
            .iter()
            .any(|(_, stream, line)| *stream == LogStream::Stdout && line == "not a protocol line")
    );
    client
}

#[test]
fn native_functions_load_initialize_and_answer() {
    let agent = Agent::start();
    let mut client = agent.connect();
    let health = client.call(Request::Health, &[]);
    assert!(matches!(health.reply, Reply::Healthy { loaded: false, .. }));
    drop(client);
    let mut client = ready_shell_agent(&agent);

    let answer = client.invoke("req-1", r#"{"name": "world"}"#);
    assert!(
        matches!(&answer.reply, Reply::Invoked { error: None, request_id, .. } if request_id == "req-1")
    );
    let body: serde_json::Value = serde_json::from_slice(&answer.payload).unwrap();
    assert!(body["length"].as_u64().unwrap() > 20);
    assert_eq!(
        answer.logs,
        vec![(
            Some("req-1".into()),
            LogStream::Stderr,
            "handling test".into()
        )]
    );

    let failed = client.invoke("req-2", r#"{"fail": true}"#);
    match failed.reply {
        Reply::Invoked {
            error: Some(error), ..
        } => assert_eq!(error.kind, "Boom"),
        other => panic!("unexpected {other:?}"),
    }
    let bad = client.invoke("req-3", "{not json");
    assert!(matches!(
        bad.reply,
        Reply::Failed {
            kind: ErrorKind::BadRequest,
            ..
        }
    ));

    let seed = [7u8; 64];
    let synced = client.call(Request::Sync { realtime_ns: 1 }, &seed);
    assert_eq!(synced.reply, Reply::Synced { applied: false });
    let reseeded = client.invoke("req-4", "{}");
    assert_eq!(reseeded.payload, br#"{"reseeded":true}"#);
    let normal = client.invoke("req-5", "{}");
    assert!(normal.payload.starts_with(br#"{"length""#));
    let short_seed = client.call(Request::Sync { realtime_ns: 1 }, &[1, 2]);
    assert!(matches!(
        short_seed.reply,
        Reply::Failed {
            kind: ErrorKind::BadRequest,
            ..
        }
    ));

    let exited = client.invoke("req-6", r#"{"exit": true}"#);
    match exited.reply {
        Reply::Failed {
            kind: ErrorKind::RuntimeExited,
            message,
        } => {
            assert!(message.contains("status 3"), "{message}")
        }
        other => panic!("unexpected {other:?}"),
    }
    let after = client.invoke("req-7", "{}");
    assert!(matches!(
        after.reply,
        Reply::Failed {
            kind: ErrorKind::RuntimeExited,
            ..
        }
    ));
}

#[test]
fn reconnecting_keeps_the_initialized_function() {
    let agent = Agent::start();
    let client = ready_shell_agent(&agent);
    drop(client);
    let mut again = agent.connect();
    let health = again.call(Request::Health, &[]);
    assert!(matches!(
        health.reply,
        Reply::Healthy {
            loaded: true,
            running: true,
            ..
        }
    ));
    assert!(matches!(
        again.invoke("r", "[1,2,3]").reply,
        Reply::Invoked { error: None, .. }
    ));
    let reload = again.call(
        Request::Load {
            function: setup(Runtime::Native, "x"),
        },
        &[],
    );
    assert!(matches!(
        reload.reply,
        Reply::Failed {
            kind: ErrorKind::BadRequest,
            ..
        }
    ));
}

#[test]
fn load_and_init_failures_are_reported() {
    let agent = Agent::start();
    let mut client = agent.connect();
    let missing = client.call(
        Request::Load {
            function: setup(Runtime::Native, "bootstrap"),
        },
        &archive(&[("other", "x", 0o644)]),
    );
    assert!(matches!(
        missing.reply,
        Reply::Failed {
            kind: ErrorKind::LoadFailed,
            ..
        }
    ));
    let garbage = client.call(
        Request::Load {
            function: setup(Runtime::Native, "bootstrap"),
        },
        b"definitely not an archive",
    );
    assert!(matches!(
        garbage.reply,
        Reply::Failed {
            kind: ErrorKind::LoadFailed,
            ..
        }
    ));
    let early = client.call(Request::Init { timeout_ms: 1000 }, &[]);
    assert!(matches!(
        early.reply,
        Reply::Failed {
            kind: ErrorKind::NotReady,
            ..
        }
    ));
    let crashing = client.call(
        Request::Load {
            function: setup(Runtime::Native, "bootstrap"),
        },
        &archive(&[(
            "bootstrap",
            "#!/bin/sh\necho 'cannot start' >&2\nexit 9\n",
            0o755,
        )]),
    );
    assert!(matches!(crashing.reply, Reply::Loaded { .. }));
    let init = client.call(Request::Init { timeout_ms: 5000 }, &[]);
    match init.reply {
        Reply::Failed {
            kind: ErrorKind::InitFailed,
            message,
        } => {
            assert!(message.contains("status 9"), "{message}")
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(init.logs.iter().any(|(_, _, line)| line == "cannot start"));
    let invoke = client.invoke("r", "{}");
    assert!(matches!(
        invoke.reply,
        Reply::Failed {
            kind: ErrorKind::NotReady,
            ..
        }
    ));
}

#[test]
fn hung_functions_time_out() {
    let agent = Agent::start();
    let mut client = agent.connect();
    client.call(
        Request::Load {
            function: setup(Runtime::Native, "bootstrap"),
        },
        &archive(&[(
            "bootstrap",
            "#!/bin/sh\necho '{\"ready\":true}'\nwhile read -r line; do sleep 30; done\n",
            0o755,
        )]),
    );
    assert!(matches!(
        client.call(Request::Init { timeout_ms: 5000 }, &[]).reply,
        Reply::Initialized { .. }
    ));
    let started = Instant::now();
    let timed_out = client.call(
        Request::Invoke {
            request_id: "slow".into(),
            deadline_ms: now_ms() + 200,
        },
        b"{}",
    );
    assert!(matches!(
        timed_out.reply,
        Reply::Failed {
            kind: ErrorKind::Timeout,
            ..
        }
    ));
    assert!(started.elapsed() < Duration::from_secs(5));
}

fn python_available() -> bool {
    Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

const PYTHON_FUNCTION: &str = r#"
import os
import random

calls = 0
print("module imported")

def handler(event, context):
    global calls
    calls += 1
    print("handling", event.get("name"))
    if event.get("raise"):
        raise ValueError("bad input")
    return {
        "hello": event.get("name"),
        "calls": calls,
        "request_id": context.request_id,
        "greeting": os.environ.get("GREETING"),
        "remaining_ok": context.get_remaining_time_in_millis() > 0,
        "random": random.random(),
    }
"#;

#[test]
fn python_functions_keep_module_state_between_invocations() {
    if !python_available() {
        eprintln!("skipping: python3 is not installed");
        return;
    }
    let agent = Agent::start();
    let mut client = agent.connect();
    let loaded = client.call(
        Request::Load {
            function: setup(Runtime::Python, "app.main.handler"),
        },
        &archive(&[
            ("app/__init__.py", "", 0o644),
            ("app/main.py", PYTHON_FUNCTION, 0o644),
        ]),
    );
    assert!(
        matches!(loaded.reply, Reply::Loaded { files: 2, .. }),
        "{:?}",
        loaded.reply
    );
    let init = client.call(Request::Init { timeout_ms: 10_000 }, &[]);
    assert!(
        matches!(init.reply, Reply::Initialized { .. }),
        "{:?} {:?}",
        init.reply,
        init.logs
    );
    assert!(
        init.logs
            .iter()
            .any(|(_, _, line)| line == "module imported")
    );
    for expected in 1..=2 {
        let answer = client.invoke(&format!("py-{expected}"), r#"{"name": "ada"}"#);
        assert!(
            matches!(answer.reply, Reply::Invoked { error: None, .. }),
            "{:?}",
            answer.reply
        );
        let body: serde_json::Value = serde_json::from_slice(&answer.payload).unwrap();
        assert_eq!(body["hello"], "ada");
        assert_eq!(body["calls"], expected);
        assert_eq!(body["greeting"], "hello");
        assert_eq!(body["remaining_ok"], true);
        assert_eq!(body["request_id"], format!("py-{expected}"));
        assert!(
            answer
                .logs
                .iter()
                .any(|(id, _, line)| line == "handling ada"
                    && id.as_deref() == Some(&format!("py-{expected}")[..]))
        );
    }
    let raised = client.invoke("py-err", r#"{"raise": true}"#);
    match raised.reply {
        Reply::Invoked {
            error: Some(error), ..
        } => {
            assert_eq!(error.kind, "ValueError");
            assert_eq!(error.message, "bad input");
            assert!(error.trace.iter().any(|line| line.contains("main.py")));
        }
        other => panic!("unexpected {other:?}"),
    }
    let still_ok = client.invoke("py-3", r#"{"name": "bob"}"#);
    assert!(matches!(still_ok.reply, Reply::Invoked { error: None, .. }));
}

#[test]
fn python_import_errors_fail_initialization() {
    if !python_available() {
        eprintln!("skipping: python3 is not installed");
        return;
    }
    let agent = Agent::start();
    let mut client = agent.connect();
    client.call(
        Request::Load {
            function: setup(Runtime::Python, "main.handler"),
        },
        &archive(&[("main.py", "import does_not_exist\n", 0o644)]),
    );
    let init = client.call(Request::Init { timeout_ms: 10_000 }, &[]);
    match init.reply {
        Reply::Failed {
            kind: ErrorKind::InitFailed,
            message,
        } => {
            assert!(message.contains("ModuleNotFoundError"), "{message}")
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(
        init.logs
            .iter()
            .any(|(_, _, line)| line.contains("does_not_exist"))
    );
}
