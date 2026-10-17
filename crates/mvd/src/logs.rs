use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use mv_proto::api::LogEntry;

pub const PER_FUNCTION: usize = 2000;

#[derive(Default)]
pub struct LogStore {
    functions: Mutex<HashMap<String, VecDeque<LogEntry>>>,
}

fn timestamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

impl LogStore {
    pub fn push(&self, function: &str, request_id: Option<&str>, stream: &str, line: &str) {
        let entry = LogEntry {
            timestamp: timestamp(),
            request_id: request_id.map(str::to_string),
            stream: stream.to_string(),
            line: line.to_string(),
        };
        let mut functions = self.functions.lock().unwrap_or_else(|e| e.into_inner());
        let entries = functions.entry(function.to_string()).or_default();
        if entries.len() == PER_FUNCTION {
            entries.pop_front();
        }
        entries.push_back(entry);
    }

    pub fn recent(&self, function: &str, limit: usize, request_id: Option<&str>) -> Vec<LogEntry> {
        let functions = self.functions.lock().unwrap_or_else(|e| e.into_inner());
        let Some(entries) = functions.get(function) else {
            return Vec::new();
        };
        let mut matching: Vec<LogEntry> = entries
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
}
