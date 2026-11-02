use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use mv_proto::agent::MAX_LOG_LINE;
use mv_proto::api::LogEntry;

pub const PER_FUNCTION: usize = mv_proto::api::MAX_LOG_ENTRIES;
/// Lines come from the guest, which may ignore the agent's limits, so the daemon bounds both
/// the line length and the total kept per function.
const BYTES_PER_FUNCTION: usize = 4 << 20;

#[derive(Default)]
struct FunctionLog {
    entries: VecDeque<LogEntry>,
    bytes: usize,
}

#[derive(Default)]
pub struct LogStore {
    functions: Mutex<HashMap<String, FunctionLog>>,
}

fn timestamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn truncate(line: &str) -> &str {
    let mut end = line.len().min(MAX_LOG_LINE);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    &line[..end]
}

impl LogStore {
    pub fn push(&self, function: &str, request_id: Option<&str>, stream: &str, line: &str) {
        let entry = LogEntry {
            timestamp: timestamp(),
            request_id: request_id.map(str::to_string),
            stream: stream.to_string(),
            line: truncate(line).to_string(),
        };
        let size = entry.line.len();
        let mut functions = self.functions.lock().unwrap_or_else(|e| e.into_inner());
        let log = functions.entry(function.to_string()).or_default();
        while log.entries.len() == PER_FUNCTION || log.bytes + size > BYTES_PER_FUNCTION {
            let Some(oldest) = log.entries.pop_front() else {
                break;
            };
            log.bytes -= oldest.line.len();
        }
        log.bytes += size;
        log.entries.push_back(entry);
    }

    pub fn recent(&self, function: &str, limit: usize, request_id: Option<&str>) -> Vec<LogEntry> {
        let functions = self.functions.lock().unwrap_or_else(|e| e.into_inner());
        let Some(log) = functions.get(function) else {
            return Vec::new();
        };
        let mut matching: Vec<LogEntry> = log
            .entries
            .iter()
            .rev()
            .filter(|e| request_id.is_none_or(|id| e.request_id.as_deref() == Some(id)))
            .take(limit)
            .cloned()
            .collect();
        matching.reverse();
        matching
    }

    pub fn forget(&self, function: &str) {
        self.functions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(function);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_most_recent_entries_per_function() {
        let logs = LogStore::default();
        for i in 0..PER_FUNCTION + 5 {
            let id = format!("r{}", i % 2);
            logs.push("a", Some(&id), "stderr", &i.to_string());
        }
        logs.push("b", None, "platform", "other");
        let recent = logs.recent("a", 3, None);
        let lines: Vec<&str> = recent.iter().map(|e| e.line.as_str()).collect();
        let last = PER_FUNCTION + 4;
        assert_eq!(
            lines,
            [
                (last - 2).to_string(),
                (last - 1).to_string(),
                last.to_string()
            ]
        );
        let odd = logs.recent("a", 2, Some("r1"));
        assert!(odd.iter().all(|e| e.request_id.as_deref() == Some("r1")));
        assert_eq!(logs.recent("a", usize::MAX, None).len(), PER_FUNCTION);
        logs.forget("a");
        assert!(logs.recent("a", 10, None).is_empty());
        assert_eq!(logs.recent("b", 10, None)[0].stream, "platform");
    }

    #[test]
    fn bounds_line_length_and_total_bytes() {
        let logs = LogStore::default();
        let long = "é".repeat(MAX_LOG_LINE);
        for _ in 0..PER_FUNCTION {
            logs.push("noisy", None, "stderr", &long);
        }
        let kept = logs.recent("noisy", usize::MAX, None);
        assert!(kept.iter().all(|e| e.line.len() <= MAX_LOG_LINE));
        let total: usize = kept.iter().map(|e| e.line.len()).sum();
        assert!(total <= BYTES_PER_FUNCTION, "{total}");
        assert!(kept.len() < PER_FUNCTION);
    }
}
