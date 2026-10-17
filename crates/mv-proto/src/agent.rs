use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

pub const AGENT_PORT: u32 = 52;
pub const ENTROPY_LEN: usize = 64;
pub const MAX_RESPONSE_LEN: usize = 6 << 20;
pub const MAX_EVENT_LEN: usize = 6 << 20;
pub const MAX_LOG_LINE: usize = 16 << 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Runtime {
    Native,
    Python,
}

impl Runtime {
    pub const ALL: [Runtime; 2] = [Runtime::Native, Runtime::Python];

    pub fn name(self) -> &'static str {
        match self {
            Runtime::Native => "native",
            Runtime::Python => "python",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|runtime| runtime.name() == name)
    }

    pub fn default_handler(self) -> &'static str {
        match self {
            Runtime::Native => "bootstrap",
            Runtime::Python => "main.handler",
        }
    }
}

impl fmt::Display for Runtime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionSetup {
    pub name: String,
    pub runtime: Runtime,
    pub handler: String,
    pub memory_mib: u32,
    pub env: BTreeMap<String, String>,
}

/// Messages from the host. `Sync` carries host entropy and `Load` the code archive as payload;
/// `Invoke` carries the event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Health,
    Sync {
        realtime_ns: u64,
    },
    Load {
        function: FunctionSetup,
    },
    Init {
        timeout_ms: u64,
    },
    Invoke {
        request_id: String,
        deadline_ms: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogStream {
    Stdout,
    Stderr,
    Agent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    BadRequest,
    LoadFailed,
    InitFailed,
    NotReady,
    RuntimeExited,
    ResponseTooLarge,
    Timeout,
    Internal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionError {
    #[serde(rename = "type")]
    pub kind: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trace: Vec<String>,
}

/// Messages from the agent. `Log` may arrive at any time; every request gets exactly one
/// other reply. A successful `Invoked` carries the function response as payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Reply {
    Healthy {
        version: String,
        uptime_ms: u64,
        loaded: bool,
        running: bool,
    },
    Synced {
        applied: bool,
    },
    Loaded {
        files: u64,
        bytes: u64,
    },
    Initialized {
        duration_ms: f64,
    },
    Invoked {
        request_id: String,
        duration_ms: f64,
        error: Option<FunctionError>,
    },
    Failed {
        kind: ErrorKind,
        message: String,
    },
    Log {
        request_id: Option<String>,
        stream: LogStream,
        line: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame;

    #[test]
    fn messages_use_tagged_snake_case_json() {
        let invoke = Request::Invoke {
            request_id: "r-1".into(),
            deadline_ms: 3000,
        };
        let json = serde_json::to_string(&invoke).unwrap();
        assert_eq!(
            json,
            r#"{"type":"invoke","request_id":"r-1","deadline_ms":3000}"#
        );
        let wire = frame::encode(&invoke, b"{}").unwrap();
        let (decoded, payload): (Request, _) = frame::read_frame(&mut wire.as_slice()).unwrap();
        assert_eq!(decoded, invoke);
        assert_eq!(payload, b"{}");
        let log: Reply = serde_json::from_str(
            r#"{"type":"log","request_id":null,"stream":"stderr","line":"hi"}"#,
        )
        .unwrap();
        assert_eq!(
            log,
            Reply::Log {
                request_id: None,
                stream: LogStream::Stderr,
                line: "hi".into()
            }
        );
        assert!(serde_json::from_str::<Request>(r#"{"type":"reboot"}"#).is_err());
    }

    #[test]
    fn runtimes_have_names_and_default_handlers() {
        for runtime in Runtime::ALL {
            assert_eq!(Runtime::parse(runtime.name()), Some(runtime));
        }
        assert_eq!(Runtime::parse("ruby"), None);
        assert_eq!(Runtime::Python.default_handler(), "main.handler");
        assert_eq!(Runtime::Native.to_string(), "native");
    }
}
