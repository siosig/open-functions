//! Per-function log ring buffer with `follow` support (T079, US5). Fed by
//! [`super::pipe::pump`]'s parsed [`super::pipe::LogRecord`]s (see
//! `runtime::process`'s and `runtime::container`'s log-draining tasks) and
//! consumed by `GET /v1/functions/{name}/logs?tail&follow` (T081).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;

use super::pipe::LogRecord;

/// Broadcast channel capacity for `follow` subscribers -- independent of the
/// ring buffer's own `tail` capacity ([`LogStore::new`]'s `capacity`), just
/// large enough that a slow follower doesn't miss lines between two `recv`
/// calls under normal load. A subscriber that falls behind past this many
/// lines gets `RecvError::Lagged` from `tokio::sync::broadcast`, which
/// callers (T081) treat as "skip ahead and keep following", not a fatal
/// stream error.
const FOLLOW_CHANNEL_CAPACITY: usize = 256;

/// Ring buffer for one function's log lines: the most recent `capacity`
/// [`LogRecord`]s, plus a broadcast sender so `follow` streams see new lines
/// as they arrive.
pub struct LogRingBuffer {
    capacity: usize,
    lines: Mutex<VecDeque<LogRecord>>,
    tx: broadcast::Sender<LogRecord>,
}

impl LogRingBuffer {
    pub fn new(capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel(FOLLOW_CHANNEL_CAPACITY);
        Self {
            capacity: capacity.max(1),
            lines: Mutex::new(VecDeque::new()),
            tx,
        }
    }

    /// Appends `record`, evicting the oldest line first if already at
    /// capacity, and notifies any live `follow` subscribers. The mutex
    /// critical section never spans an `.await`, so this is safe to call
    /// from a hot log-draining loop.
    pub fn push(&self, record: LogRecord) {
        {
            let mut lines = self
                .lines
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if lines.len() >= self.capacity {
                lines.pop_front();
            }
            lines.push_back(record.clone());
        }
        // No subscribers is not an error: `follow` is opt-in, and `tail`
        // reads from `lines` above regardless of whether anyone is
        // currently following.
        let _ = self.tx.send(record);
    }

    /// The most recent `n` lines, oldest first. `n` greater than the number
    /// of retained lines just returns everything currently retained.
    pub fn tail(&self, n: usize) -> Vec<LogRecord> {
        let lines = self
            .lines
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let len = lines.len();
        let skip = len.saturating_sub(n);
        lines.iter().skip(skip).cloned().collect()
    }

    /// Subscribes to lines pushed from this point on, for `follow=true`.
    pub fn subscribe(&self) -> broadcast::Receiver<LogRecord> {
        self.tx.subscribe()
    }
}

/// Registry of per-function [`LogRingBuffer`]s, all sharing the same
/// configured `capacity` (`log.function_ring_buffer_lines`). Buffers are
/// created lazily on first use ([`LogStore::buffer_for`]) and dropped on
/// function delete ([`LogStore::remove`]) -- an in-flight `follow` stream
/// holding its own `Arc` clone from before the removal keeps working until
/// the client disconnects; a *new* lookup after removal gets a fresh, empty
/// buffer.
pub struct LogStore {
    capacity: usize,
    buffers: Mutex<HashMap<String, Arc<LogRingBuffer>>>,
}

impl LogStore {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            buffers: Mutex::new(HashMap::new()),
        }
    }

    /// Returns (creating if this is the first line/lookup for this
    /// function) the ring buffer for `function_name`.
    pub fn buffer_for(&self, function_name: &str) -> Arc<LogRingBuffer> {
        let mut buffers = self
            .buffers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(
            buffers
                .entry(function_name.to_string())
                .or_insert_with(|| Arc::new(LogRingBuffer::new(self.capacity))),
        )
    }

    /// Forgets the ring buffer for a deleted function, per T080's delete
    /// flow.
    pub fn remove(&self, function_name: &str) {
        self.buffers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(function_name);
    }
}

impl Default for LogStore {
    /// `1000`, matching `ops-config.md`'s `log.function_ring_buffer_lines`
    /// default -- convenient for tests and call sites that don't have a
    /// config value handy; real `cf-rs serve` construction passes the
    /// configured value explicitly via [`LogStore::new`].
    fn default() -> Self {
        Self::new(1000)
    }
}
