//! PTY output broadcasting: scrollback ring buffer, subscriber fanout,
//! and pluggable state classification.

use crate::classify::{self, ProcessState, StateClassifier};
use crate::config::Config;
use bytes::Bytes;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use tokio::sync::broadcast;

/// Monotonic clock epoch — set once at startup.
static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

fn epoch() -> Instant {
    *EPOCH.get_or_init(Instant::now)
}

/// Current monotonic milliseconds since epoch.
pub fn now_millis() -> u64 {
    epoch().elapsed().as_millis() as u64
}

// -- Shared output state --

/// Shared state for the pty output stream.
pub struct OutputState {
    /// Broadcast channel for live output to subscribers.
    pub tx: broadcast::Sender<Bytes>,
    /// Monotonic timestamp (millis) of last pty output byte.
    pub last_output_at: AtomicU64,
    /// Scrollback ring buffer for late-joining clients.
    pub scrollback: Mutex<Scrollback>,
    /// State classifier.
    pub classifier: Mutex<Box<dyn StateClassifier>>,
}

impl OutputState {
    pub fn new(config: &Config) -> Self {
        let (tx, _) = broadcast::channel(256);
        let classifier = classify::from_config(&config.classifier);
        Self {
            tx,
            last_output_at: AtomicU64::new(now_millis()),
            scrollback: Mutex::new(Scrollback::new(config.scrollback_bytes)),
            classifier: Mutex::new(classifier),
        }
    }

    /// Record output: update timestamp, classify state, push to scrollback, broadcast.
    pub fn push(&self, chunk: Bytes) {
        let now = now_millis();
        let byte_count = chunk.len();
        self.last_output_at.store(now, Ordering::Relaxed);
        self.classifier.lock().unwrap().record(byte_count, now);
        self.scrollback.lock().unwrap().push(chunk.clone());
        let _ = self.tx.send(chunk);
    }

    /// Milliseconds since last pty output.
    pub fn idle_ms(&self) -> u32 {
        let last = self.last_output_at.load(Ordering::Relaxed);
        let now = now_millis();
        // Saturate at u32::MAX (~49 days) rather than wrapping.
        now.saturating_sub(last).min(u32::MAX as u64) as u32
    }

    /// Current process state.
    pub fn process_state(&self) -> ProcessState {
        let now = now_millis();
        let mut cls = self.classifier.lock().unwrap();
        cls.tick(now);
        cls.state()
    }

    /// Milliseconds in current state.
    pub fn state_ms(&self) -> u32 {
        let now = now_millis();
        let mut cls = self.classifier.lock().unwrap();
        cls.tick(now);
        cls.state_ms(now)
    }

    /// Mark the process as dead.
    pub fn set_dead(&self) {
        let now = now_millis();
        self.classifier.lock().unwrap().set_dead(now);
    }

    /// Get a snapshot of the scrollback buffer for a new subscriber.
    pub fn scrollback_snapshot(&self) -> Vec<Bytes> {
        self.scrollback.lock().unwrap().snapshot()
    }
}

/// Fixed-capacity ring buffer for recent pty output.
pub struct Scrollback {
    buf: VecDeque<Bytes>,
    total_bytes: usize,
    max_bytes: usize,
}

impl Scrollback {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            buf: VecDeque::new(),
            total_bytes: 0,
            max_bytes,
        }
    }

    pub fn push(&mut self, chunk: Bytes) {
        self.total_bytes += chunk.len();
        self.buf.push_back(chunk);
        while self.total_bytes > self.max_bytes {
            if let Some(old) = self.buf.pop_front() {
                self.total_bytes -= old.len();
            } else {
                break;
            }
        }
    }

    /// Return a snapshot of all buffered chunks (non-destructive).
    pub fn snapshot(&self) -> Vec<Bytes> {
        self.buf.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrollback_eviction() {
        let mut sb = Scrollback::new(10);
        sb.push(Bytes::from_static(b"hello")); // 5 bytes
        sb.push(Bytes::from_static(b"world")); // 10 bytes total
        assert_eq!(sb.snapshot().len(), 2);

        sb.push(Bytes::from_static(b"!")); // 11 bytes -> evict "hello"
        assert_eq!(sb.snapshot().len(), 2);
        assert_eq!(sb.total_bytes, 6); // "world" + "!"
    }

    #[test]
    fn output_state_idle_ms() {
        let config = Config::default();
        let state = OutputState::new(&config);
        assert!(state.idle_ms() < 100);
    }

    /// Issue #8: scrollback with max_bytes=0 should work (everything gets evicted).
    #[test]
    fn scrollback_zero_max_bytes() {
        let mut sb = Scrollback::new(0);
        sb.push(Bytes::from_static(b"data"));
        // Everything should be evicted immediately since max_bytes is 0.
        assert_eq!(sb.total_bytes, 0);
        assert!(sb.snapshot().is_empty());
    }

    /// Issue #8: scrollback_snapshot returns chunks in insertion order.
    #[test]
    fn scrollback_snapshot_order() {
        let mut sb = Scrollback::new(1024);
        sb.push(Bytes::from_static(b"first"));
        sb.push(Bytes::from_static(b"second"));
        sb.push(Bytes::from_static(b"third"));

        let snapshot = sb.snapshot();
        assert_eq!(snapshot.len(), 3);
        assert_eq!(snapshot[0].as_ref(), b"first");
        assert_eq!(snapshot[1].as_ref(), b"second");
        assert_eq!(snapshot[2].as_ref(), b"third");
    }

    /// Scrollback eviction preserves order of remaining chunks.
    #[test]
    fn scrollback_eviction_preserves_order() {
        let mut sb = Scrollback::new(10);
        sb.push(Bytes::from_static(b"aaaa")); // 4
        sb.push(Bytes::from_static(b"bbbb")); // 8
        sb.push(Bytes::from_static(b"cccc")); // 12 -> evict "aaaa" -> 8

        let snapshot = sb.snapshot();
        assert_eq!(snapshot.len(), 2);
        assert_eq!(snapshot[0].as_ref(), b"bbbb");
        assert_eq!(snapshot[1].as_ref(), b"cccc");
    }

    /// Scrollback handles a single large chunk that exceeds max_bytes.
    #[test]
    fn scrollback_single_oversized_chunk() {
        let mut sb = Scrollback::new(5);
        sb.push(Bytes::from_static(b"toolongchunk")); // 12 bytes > 5
        // The push adds it, then evicts until under max. Since it's the only
        // chunk and its size exceeds max, it gets evicted too.
        assert_eq!(sb.total_bytes, 0);
        assert!(sb.snapshot().is_empty());
    }

    /// total_bytes stays consistent with actual buffer contents after many evictions.
    /// Catches: off-by-one in the eviction loop where total_bytes drifts from reality.
    #[test]
    fn scrollback_total_bytes_consistent_after_many_evictions() {
        let mut sb = Scrollback::new(100);
        // Push a mix of sizes to trigger many evictions.
        for i in 0u8..200 {
            let size = (i % 30) as usize + 1; // 1..30 bytes
            sb.push(Bytes::from(vec![i; size]));
            let actual_total: usize = sb.buf.iter().map(|c| c.len()).sum();
            assert_eq!(
                sb.total_bytes, actual_total,
                "total_bytes drifted after push #{i}"
            );
        }
    }

    /// snapshot() does not consume the buffer.
    #[test]
    fn scrollback_snapshot_is_nondestructive() {
        let mut sb = Scrollback::new(1024);
        sb.push(Bytes::from_static(b"data"));
        let first = sb.snapshot();
        let second = sb.snapshot();
        assert_eq!(first.len(), second.len());
        assert_eq!(first[0], second[0]);
    }
}
