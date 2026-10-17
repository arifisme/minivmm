use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mv_proto::agent::{
    ErrorKind, FunctionSetup, LogStream, MAX_EVENT_LEN, Reply, Request, Runtime,
};
use mv_proto::frame::{self, FrameError};
use serde_json::value::RawValue;

use crate::archive;
use crate::logs::LogSink;
use crate::runtime::{InvokeOutcome, Launch, RuntimeFailure, RuntimeProcess};
use crate::system::{self, Reaper};
use crate::transport::Listener;

const BOOTSTRAP: &str = include_str!("bootstrap.py");
const MIN_SEED_LEN: usize = 16;
const MAX_SEED_LEN: usize = 4096;
const INVOKE_GRACE: Duration = Duration::from_secs(1);
const SYSTEM_PATH: &str = "/usr/local/bin:/usr/bin:/bin:/usr/local/sbin:/usr/sbin:/sbin";

type Outcome = Result<(Reply, Vec<u8>), (ErrorKind, String)>;

pub struct Agent {
    started: Instant,
    guest: bool,
    work_dir: PathBuf,
    function: Option<FunctionSetup>,
    process: Option<RuntimeProcess>,
    logs: Arc<LogSink>,
    reaper: Arc<Reaper>,
    reseed_pending: bool,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn reply(reply: Reply) -> Outcome {
    Ok((reply, Vec::new()))
}

impl Agent {
    pub fn new(guest: bool, work_dir: PathBuf, reaper: Arc<Reaper>) -> Self {
        Self {
            started: Instant::now(),
            guest,
            work_dir,
            function: None,
            process: None,
            logs: LogSink::new(),
            reaper,
            reseed_pending: false,
        }
    }

    pub fn serve(&mut self, listener: &Listener) -> io::Result<()> {
        loop {
            let (reader, writer) = listener.accept()?;
            self.logs.attach(writer);
            self.handle_connection(reader);
            self.logs.detach();
        }
    }

    fn handle_connection(&mut self, mut reader: Box<dyn Read + Send>) {
        loop {
            let (request, payload) = match frame::read_frame::<_, Request>(&mut reader) {
                Ok(frame) => frame,
                Err(FrameError::Malformed(err)) => {
                    let failed = Reply::Failed {
                        kind: ErrorKind::BadRequest,
                        message: format!("malformed request: {err}"),
                    };
                    if self.logs.send(&failed, &[]).is_err() {
                        return;
                    }
                    continue;
                }
                Err(_) => return,
            };
            let (reply, payload) = self
                .handle(request, payload)
                .unwrap_or_else(|(kind, message)| (Reply::Failed { kind, message }, Vec::new()));
            if self.logs.send(&reply, &payload).is_err() {
                return;
            }
        }
    }

    fn handle(&mut self, request: Request, payload: Vec<u8>) -> Outcome {
        match request {
            Request::Health => reply(Reply::Healthy {
                version: env!("CARGO_PKG_VERSION").to_string(),
                uptime_ms: self.started.elapsed().as_millis() as u64,
                loaded: self.function.is_some(),
                running: self
                    .process
                    .as_ref()
                    .is_some_and(RuntimeProcess::is_running),
            }),
            Request::Sync { realtime_ns } => self.sync(realtime_ns, &payload),
            Request::Load { function } => self.load(function, &payload),
            Request::Init { timeout_ms } => self.init(Duration::from_millis(timeout_ms)),
            Request::Invoke {
                request_id,
                deadline_ms,
            } => self.invoke(&request_id, deadline_ms, payload),
        }
    }

    fn sync(&mut self, realtime_ns: u64, seed: &[u8]) -> Outcome {
        if !(MIN_SEED_LEN..=MAX_SEED_LEN).contains(&seed.len()) {
            return Err((
                ErrorKind::BadRequest,
                format!("entropy must be {MIN_SEED_LEN} to {MAX_SEED_LEN} bytes"),
            ));
        }
        if self.guest {
            system::add_entropy(seed).map_err(|e| {
                (
                    ErrorKind::Internal,
                    format!("cannot reseed the kernel: {e}"),
                )
            })?;
            system::set_realtime(realtime_ns)
                .map_err(|e| (ErrorKind::Internal, format!("cannot set the clock: {e}")))?;
        }
        self.reseed_pending = true;
        reply(Reply::Synced {
            applied: self.guest,
        })
    }

    fn code_dir(&self) -> PathBuf {
        self.work_dir.join("function")
    }

    fn load(&mut self, function: FunctionSetup, archive: &[u8]) -> Outcome {
        if self.function.is_some() {
            return Err((ErrorKind::BadRequest, "a function is already loaded".into()));
        }
        let dir = self.code_dir();
        let failed = |message: String| (ErrorKind::LoadFailed, message);
        fs::create_dir_all(&dir)
            .map_err(|e| failed(format!("cannot create {}: {e}", dir.display())))?;
        let extracted = match archive::extract(archive, &dir) {
            Ok(extracted) => extracted,
            Err(message) => {
                let _ = fs::remove_dir_all(&dir);
                return Err(failed(message));
            }
        };
        if function.runtime == Runtime::Native {
            let handler = dir.join(&function.handler);
            if !handler.is_file() {
                let _ = fs::remove_dir_all(&dir);
                return Err(failed(format!(
                    "the archive has no executable {}",
                    function.handler
                )));
            }
        }
        self.function = Some(function);
        reply(Reply::Loaded {
            files: extracted.files,
            bytes: extracted.bytes,
        })
    }

    fn launch(&self, function: &FunctionSetup) -> io::Result<Launch> {
        let mut env: BTreeMap<String, String> = function.env.clone();
        env.insert("PATH".into(), SYSTEM_PATH.into());
        env.insert("HOME".into(), "/tmp".into());
        env.insert("LANG".into(), "C.UTF-8".into());
        env.insert("TZ".into(), "UTC".into());
        env.insert("MV_FUNCTION_NAME".into(), function.name.clone());
        env.insert(
            "MV_FUNCTION_MEMORY_MB".into(),
            function.memory_mib.to_string(),
        );
        env.insert("MV_HANDLER".into(), function.handler.clone());
        let dir = self.code_dir();
        Ok(match function.runtime {
            Runtime::Native => Launch {
                program: dir.join(&function.handler),
                args: Vec::new(),
                dir,
                env,
            },
            Runtime::Python => {
                let bootstrap = self.work_dir.join("bootstrap.py");
                fs::write(&bootstrap, BOOTSTRAP)?;
                let (module, entry) = function
                    .handler
                    .rsplit_once('.')
                    .ok_or_else(|| io::Error::other("python handlers look like module.function"))?;
                env.insert("PYTHONDONTWRITEBYTECODE".into(), "1".into());
                Launch {
                    program: PathBuf::from("python3"),
                    args: vec![
                        bootstrap.display().to_string(),
                        module.to_string(),
                        entry.to_string(),
                    ],
                    dir,
                    env,
                }
            }
        })
    }

    fn init(&mut self, timeout: Duration) -> Outcome {
        let Some(function) = self.function.clone() else {
            return Err((ErrorKind::NotReady, "no function is loaded".into()));
        };
        if self.process.is_some() {
            return Err((
                ErrorKind::BadRequest,
                "the function is already initialized".into(),
            ));
        }
        let started = Instant::now();
        let init_failed = |message: String| (ErrorKind::InitFailed, message);
        let launch = self
            .launch(&function)
            .map_err(|e| init_failed(format!("cannot prepare the runtime: {e}")))?;
        let mut process = RuntimeProcess::spawn(&launch, self.logs.clone(), self.reaper.clone())
            .map_err(|e| init_failed(format!("cannot start {}: {e}", launch.program.display())))?;
        match process.wait_ready(timeout) {
            Ok(()) => {
                self.process = Some(process);
                reply(Reply::Initialized {
                    duration_ms: started.elapsed().as_secs_f64() * 1000.0,
                })
            }
            Err(failure) => {
                if let RuntimeFailure::InitError(error) = &failure {
                    for line in &error.trace {
                        self.logs.log(LogStream::Stderr, line.trim_end());
                    }
                }
                Err(init_failed(failure.to_string()))
            }
        }
    }

    fn invoke(&mut self, request_id: &str, deadline_ms: u64, event: Vec<u8>) -> Outcome {
        let Some(process) = self.process.as_mut().filter(|p| p.is_running()) else {
            let kind = if self.process.is_some() {
                ErrorKind::RuntimeExited
            } else {
                ErrorKind::NotReady
            };
            return Err((kind, "the function is not running".into()));
        };
        if event.len() > MAX_EVENT_LEN {
            return Err((ErrorKind::BadRequest, "the event is too large".into()));
        }
        let event = String::from_utf8(event)
            .ok()
            .and_then(|text| RawValue::from_string(text).ok())
            .ok_or_else(|| {
                (
                    ErrorKind::BadRequest,
                    "the event is not valid JSON".to_string(),
                )
            })?;
        let timeout = Duration::from_millis(deadline_ms.saturating_sub(now_ms())) + INVOKE_GRACE;
        let reseed = std::mem::take(&mut self.reseed_pending);
        self.logs.set_request(Some(request_id.to_string()));
        let started = Instant::now();
        let outcome = process.invoke(request_id, deadline_ms, reseed, &event, timeout);
        let duration_ms = started.elapsed().as_secs_f64() * 1000.0;
        self.logs.set_request(None);
        let invoked = |error| Reply::Invoked {
            request_id: request_id.to_string(),
            duration_ms,
            error,
        };
        match outcome {
            Ok(InvokeOutcome::Response(body)) => Ok((invoked(None), body)),
            Ok(InvokeOutcome::FunctionError(error)) => reply(invoked(Some(error))),
            Err(failure) => {
                let kind = match &failure {
                    RuntimeFailure::Exited(_) => ErrorKind::RuntimeExited,
                    RuntimeFailure::TooLarge => ErrorKind::ResponseTooLarge,
                    RuntimeFailure::Timeout => ErrorKind::Timeout,
                    RuntimeFailure::InitError(_) | RuntimeFailure::Io(_) => ErrorKind::Internal,
                };
                process.kill();
                Err((kind, failure.to_string()))
            }
        }
    }
}
