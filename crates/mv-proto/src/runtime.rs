//! The line protocol between the agent and a function process: the process prints
//! `{"ready":true}` once initialized, then answers each request line on stdin with one
//! `{"response":...}` or `{"error":{...}}` line on stdout.

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::agent::FunctionError;

#[derive(Serialize)]
pub struct InvokeLine<'a> {
    pub request_id: &'a str,
    pub deadline_ms: u64,
    pub reseed: bool,
    pub event: &'a RawValue,
}

#[derive(Debug)]
pub enum RuntimeLine {
    Ready { ready: bool },
    Response { response: Box<RawValue> },
    Error { error: FunctionError },
}

#[derive(Deserialize)]
struct Fields {
    ready: Option<bool>,
    response: Option<Box<RawValue>>,
    error: Option<FunctionError>,
}

impl RuntimeLine {
    pub fn parse(line: &[u8]) -> Option<Self> {
        let fields: Fields = serde_json::from_slice(line).ok()?;
        match (fields.ready, fields.response, fields.error) {
            (Some(ready), None, None) => Some(RuntimeLine::Ready { ready }),
            (None, Some(response), None) => Some(RuntimeLine::Response { response }),
            (None, None, Some(error)) => Some(RuntimeLine::Error { error }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_runtime_lines() {
        assert!(matches!(
            RuntimeLine::parse(br#"{"ready":true}"#),
            Some(RuntimeLine::Ready { ready: true })
        ));
        match RuntimeLine::parse(br#"{"response":{"a":[1,2]}}"#) {
            Some(RuntimeLine::Response { response }) => {
                assert_eq!(response.get(), r#"{"a":[1,2]}"#)
            }
            other => panic!("unexpected {other:?}"),
        }
        match RuntimeLine::parse(br#"{"error":{"type":"ValueError","message":"bad"}}"#) {
            Some(RuntimeLine::Error { error }) => {
                assert_eq!(error.kind, "ValueError");
                assert!(error.trace.is_empty());
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(RuntimeLine::parse(b"hello").is_none());
        assert!(RuntimeLine::parse(br#"{"other":1}"#).is_none());
        assert!(RuntimeLine::parse(br#"{"ready":true,"response":1}"#).is_none());
    }

    #[test]
    fn invoke_lines_embed_the_raw_event() {
        let event = RawValue::from_string(r#"{"x": 1}"#.into()).unwrap();
        let line = InvokeLine {
            request_id: "r",
            deadline_ms: 5,
            reseed: true,
            event: &event,
        };
        assert_eq!(
            serde_json::to_string(&line).unwrap(),
            r#"{"request_id":"r","deadline_ms":5,"reseed":true,"event":{"x": 1}}"#
        );
    }
}
