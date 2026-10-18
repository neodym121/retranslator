//! Ring buffer of recent requests. Metadata only: no bodies, no keys.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde::Serialize;

const CAPACITY: usize = 1000;

#[derive(Debug, Clone, Serialize, Default)]
pub struct LogEntry {
    pub id: u64,
    /// RFC 3339, local time.
    pub time: String,
    pub method: String,
    pub path: String,
    pub local_model: String,
    pub upstream_model: String,
    pub provider: String,
    pub stream: bool,
    pub status: u16,
    pub duration_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct Logs {
    entries: Mutex<VecDeque<LogEntry>>,
    next_id: AtomicU64,
    total: AtomicU64,
}

impl Logs {
    pub fn push(&self, mut entry: LogEntry) -> LogEntry {
        entry.id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        if entry.time.is_empty() {
            entry.time = chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, false);
        }
        self.total.fetch_add(1, Ordering::Relaxed);
        let mut entries = self.entries.lock().unwrap();
        if entries.len() >= CAPACITY {
            entries.pop_front();
        }
        entries.push_back(entry.clone());
        entry
    }

    /// Newest first.
    pub fn list(&self) -> Vec<LogEntry> {
        self.entries.lock().unwrap().iter().rev().cloned().collect()
    }

    pub fn clear(&self) {
        self.entries.lock().unwrap().clear();
    }

    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_newest_entries() {
        let logs = Logs::default();
        for i in 0..(CAPACITY + 5) {
            logs.push(LogEntry { status: 200, duration_ms: i as u64, ..Default::default() });
        }
        let list = logs.list();
        assert_eq!(list.len(), CAPACITY);
        assert_eq!(list[0].duration_ms, (CAPACITY + 4) as u64);
        assert_eq!(logs.total(), (CAPACITY + 5) as u64);
        assert!(!list[0].time.is_empty());
        logs.clear();
        assert!(logs.list().is_empty());
    }
}
