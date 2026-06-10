use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use parking_lot::Mutex;
use serde::Serialize;

const MAX_IN_MEMORY: usize = 2_000;

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Info,
    Warn,
    Error,
    Dry,
    Real,
}

impl LogLevel {
    fn as_str(self) -> &'static str {
        match self {
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERROR",
            LogLevel::Dry => "DRY",
            LogLevel::Real => "REAL",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct LogEntry {
    pub ts: String,
    pub level: LogLevel,
    pub message: String,
}

/// In-memory ring buffer + append-only log file. The buffer is what the UI reads
/// via `get_logs`; the file is the durable record across restarts.
#[derive(Clone)]
pub struct LogSink {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    buf: VecDeque<LogEntry>,
    // Count of entries evicted from the front of `buf`. The absolute index of
    // `buf[0]` is `dropped`, so `since` stays a stable cursor even after the
    // ring buffer wraps — without it the UI feed freezes past MAX_IN_MEMORY.
    dropped: usize,
    path: PathBuf,
}

impl LogSink {
    pub fn new(path: PathBuf) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                buf: VecDeque::with_capacity(MAX_IN_MEMORY),
                dropped: 0,
                path,
            })),
        }
    }

    pub fn log(&self, level: LogLevel, message: impl Into<String>) {
        let entry = LogEntry {
            ts: Utc::now().to_rfc3339(),
            level,
            message: message.into(),
        };
        let line = format!("{} [{}] {}\n", entry.ts, level.as_str(), entry.message);

        let mut guard = self.inner.lock();
        if guard.buf.len() >= MAX_IN_MEMORY {
            guard.buf.pop_front();
            guard.dropped += 1;
        }
        guard.buf.push_back(entry);

        if let Ok(mut f) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&guard.path)
        {
            let _ = f.write_all(line.as_bytes());
        }
    }

    pub fn info(&self, message: impl Into<String>) {
        self.log(LogLevel::Info, message);
    }

    pub fn warn(&self, message: impl Into<String>) {
        self.log(LogLevel::Warn, message);
    }

    pub fn error(&self, message: impl Into<String>) {
        self.log(LogLevel::Error, message);
    }

    pub fn dry(&self, message: impl Into<String>) {
        self.log(LogLevel::Dry, message);
    }

    pub fn real(&self, message: impl Into<String>) {
        self.log(LogLevel::Real, message);
    }

    pub fn tail(&self, since_index: usize) -> (Vec<LogEntry>, usize) {
        let guard = self.inner.lock();
        // `total` is the count of all entries ever appended (a monotonic cursor
        // the UI stores and passes back as `since_index`), not the buffer length.
        let total = guard.dropped + guard.buf.len();
        // Translate the absolute cursor into an offset within the live buffer.
        // Anything older than `dropped` was evicted, so clamp to the buffer start.
        let from = since_index
            .saturating_sub(guard.dropped)
            .min(guard.buf.len());
        let entries = guard.buf.iter().skip(from).cloned().collect();
        (entries, total)
    }

    pub fn clear(&self) {
        let mut guard = self.inner.lock();
        guard.buf.clear();
        guard.dropped = 0;
    }
}
