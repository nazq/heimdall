//! Claude Code state classifier.
//!
//! Analyses pty output patterns using a sliding window to distinguish between
//! idle, thinking (spinner), streaming (token output), and tool use (large bursts).

use super::{OutputEvent, ProcessState, StateClassifier};
use std::collections::VecDeque;

/// Sliding window size for pattern analysis.
const WINDOW_SIZE: usize = 20;

/// Burst size threshold for tool_use detection.
const TOOL_USE_BURST_BYTES: usize = 4096;

/// Full state machine classifier tuned for Claude Code output patterns.
pub struct ClaudeClassifier {
    window: VecDeque<OutputEvent>,
    current_state: ProcessState,
    state_entered_at: u64,
    pending_state: Option<(ProcessState, u64)>,
    idle_threshold_ms: u64,
    debounce_ms: u64,
}

impl ClaudeClassifier {
    pub fn new(idle_threshold_ms: u64, debounce_ms: u64) -> Self {
        Self {
            window: VecDeque::with_capacity(WINDOW_SIZE),
            current_state: ProcessState::Idle,
            state_entered_at: 0,
            pending_state: None,
            idle_threshold_ms,
            debounce_ms,
        }
    }

    fn raw_classify(&self, now: u64) -> ProcessState {
        // Check for idle: no output for >= threshold.
        let last_ts = self.window.back().map_or(0, |e| e.timestamp_ms);
        if now.saturating_sub(last_ts) >= self.idle_threshold_ms {
            return ProcessState::Idle;
        }

        // Check for tool use: any recent burst > 4KB.
        for event in self.window.iter().rev().take(5) {
            if event.byte_count >= TOOL_USE_BURST_BYTES {
                return ProcessState::ToolUse;
            }
        }

        // Need at least a few events for pattern detection.
        if self.window.len() < 3 {
            return ProcessState::Thinking;
        }

        // Check for tool use pattern: pause > 200ms followed by burst > 1KB.
        // Iterate pairs directly from the deque — no Vec allocation needed.
        if self.window.len() >= 2 {
            let skip = self.window.len().saturating_sub(5);
            let mut iter = self.window.iter().skip(skip);
            if let Some(mut prev) = iter.next() {
                for event in iter {
                    let gap = event.timestamp_ms.saturating_sub(prev.timestamp_ms);
                    if gap > 200 && event.byte_count > 1024 {
                        return ProcessState::ToolUse;
                    }
                    prev = event;
                }
            }
        }

        // Analyse last 10 bursts for spinner vs streaming.
        // Compute statistics in a single pass — no Vec allocations.
        let skip = self.window.len().saturating_sub(10);
        let recent_count = self.window.len() - skip;
        if recent_count >= 5 {
            // Single pass: accumulate size sum, size sum-of-squares, gap sum.
            let mut size_sum = 0.0_f64;
            let mut size_sq_sum = 0.0_f64;
            let mut gap_sum = 0.0_f64;
            let mut gap_count = 0u32;
            let mut prev_ts = 0u64;
            let mut first = true;

            for event in self.window.iter().skip(skip) {
                let s = event.byte_count as f64;
                size_sum += s;
                size_sq_sum += s * s;
                if !first {
                    gap_sum += event.timestamp_ms.saturating_sub(prev_ts) as f64;
                    gap_count += 1;
                }
                prev_ts = event.timestamp_ms;
                first = false;
            }

            let n = recent_count as f64;
            let mean_size = size_sum / n;
            // Var = E[X^2] - E[X]^2 (numerically stable enough for our ranges).
            let variance = (size_sq_sum / n) - (mean_size * mean_size);
            let stddev = if variance > 0.0 { variance.sqrt() } else { 0.0 };

            let mean_gap = if gap_count > 0 {
                gap_sum / gap_count as f64
            } else {
                0.0
            };

            // Spinner: uniform small bursts (40-120 bytes), regular intervals (30-200ms).
            if (40.0..=120.0).contains(&mean_size)
                && stddev < 30.0
                && (30.0..=200.0).contains(&mean_gap)
            {
                return ProcessState::Thinking;
            }

            // Streaming: variable-size bursts at high frequency.
            if (mean_size > 100.0 || stddev > 50.0) && mean_gap < 200.0 {
                return ProcessState::Streaming;
            }
        }

        // Default to thinking if we have recent output but can't classify.
        ProcessState::Thinking
    }

    fn apply_debounce(&mut self, raw: ProcessState, now: u64) {
        // Dead is terminal — never transition out of it.
        if self.current_state == ProcessState::Dead {
            self.pending_state = None;
            return;
        }

        if raw == self.current_state {
            self.pending_state = None;
            return;
        }

        // Idle transitions are instant (silence is unambiguous).
        if raw == ProcessState::Idle {
            self.current_state = ProcessState::Idle;
            self.state_entered_at = now;
            self.pending_state = None;
            return;
        }

        match self.pending_state {
            Some((pending, since)) if pending == raw => {
                if now.saturating_sub(since) >= self.debounce_ms {
                    self.current_state = raw;
                    self.state_entered_at = now;
                    self.pending_state = None;
                }
            }
            _ => {
                self.pending_state = Some((raw, now));
            }
        }
    }
}

impl StateClassifier for ClaudeClassifier {
    fn record(&mut self, byte_count: usize, now_ms: u64) {
        self.window.push_back(OutputEvent {
            timestamp_ms: now_ms,
            byte_count,
        });
        while self.window.len() > WINDOW_SIZE {
            self.window.pop_front();
        }
        let raw = self.raw_classify(now_ms);
        self.apply_debounce(raw, now_ms);
    }

    fn tick(&mut self, now_ms: u64) {
        let raw = self.raw_classify(now_ms);
        self.apply_debounce(raw, now_ms);
    }

    fn state(&self) -> ProcessState {
        self.current_state
    }

    fn state_ms(&self, now_ms: u64) -> u32 {
        now_ms.saturating_sub(self.state_entered_at) as u32
    }

    fn set_dead(&mut self, now_ms: u64) {
        self.current_state = ProcessState::Dead;
        self.state_entered_at = now_ms;
        self.pending_state = None;
    }
}

#[cfg(test)]
impl ClaudeClassifier {
    fn state_name(&self, state: u8) -> &'static str {
        match state {
            0x00 => "idle",
            0x01 => "thinking",
            0x02 => "streaming",
            0x03 => "tool_use",
            0x04 => "active",
            0xFF => "dead",
            _ => "unknown",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_use_on_large_burst() {
        let mut c = ClaudeClassifier::new(3000, 200);
        c.record(50, 1000);
        c.record(5000, 1300);
        let raw = c.raw_classify(1300);
        assert_eq!(raw, ProcessState::ToolUse);
    }

    #[test]
    fn thinking_on_spinner() {
        let mut c = ClaudeClassifier::new(3000, 200);
        // Simulate spinner: uniform ~70 byte bursts at ~80ms intervals.
        for i in 0..15 {
            c.window.push_back(OutputEvent {
                timestamp_ms: 1000 + i * 80,
                byte_count: 70,
            });
        }
        // Force a recent timestamp so it's not idle.
        if let Some(last) = c.window.back_mut() {
            last.timestamp_ms = 2200;
        }
        let raw = c.raw_classify(2200);
        assert_eq!(raw, ProcessState::Thinking);
    }

    #[test]
    fn dead_state() {
        let mut c = ClaudeClassifier::new(3000, 200);
        c.set_dead(5000);
        assert_eq!(c.state(), ProcessState::Dead);
    }

    /// Issue #6: claude classifier never emits Active (0x04).
    #[test]
    fn claude_never_emits_active() {
        let mut c = ClaudeClassifier::new(3000, 200);

        // Simulate a variety of output patterns.
        // Small regular bursts (thinking pattern):
        for i in 0..20 {
            c.record(70, 1000 + i * 80);
        }
        assert_ne!(
            c.state(),
            ProcessState::Active,
            "thinking should not be Active"
        );

        // Large bursts (tool use):
        c.record(5000, 3000);
        // Even with debounce, raw state should not be Active.
        let raw = c.raw_classify(3000);
        assert_ne!(
            raw,
            ProcessState::Active,
            "raw_classify must never return Active"
        );

        // Variable bursts (streaming):
        let mut c2 = ClaudeClassifier::new(3000, 0);
        for i in 0u64..15 {
            let size = 200 + (i as usize % 5) * 100;
            c2.record(size, 1000 + i * 50);
        }
        let raw2 = c2.raw_classify(1000 + 14 * 50);
        assert_ne!(
            raw2,
            ProcessState::Active,
            "streaming raw should not be Active"
        );

        // Idle:
        let mut c3 = ClaudeClassifier::new(3000, 200);
        c3.record(100, 1000);
        c3.tick(5000);
        assert_ne!(
            c3.state(),
            ProcessState::Active,
            "idle should not be Active"
        );
    }

    /// Issue #9: debounce — non-idle transitions are NOT instant.
    #[test]
    fn debounce_delays_non_idle_transitions() {
        let mut c = ClaudeClassifier::new(3000, 200);
        assert_eq!(c.state(), ProcessState::Idle);

        // First record: raw classifies as Thinking, but debounce holds it.
        c.record(70, 1000);
        // With debounce_ms=200, the first record just sets pending_state.
        // State should still be Idle (or may transition if pending resolves).
        // Record again at 1001 — only 1ms later, debounce not expired.
        let mut c2 = ClaudeClassifier::new(3000, 200);
        c2.record(70, 1000);
        let state_after_first = c2.state();
        // After a single record, the debounce hasn't expired so state stays Idle.
        assert_eq!(
            state_after_first,
            ProcessState::Idle,
            "first non-idle event should not transition instantly"
        );
    }

    /// Issue #9: debounce — idle transitions ARE instant.
    #[test]
    fn idle_transition_is_instant() {
        let mut c = ClaudeClassifier::new(3000, 200);

        // Get into Thinking state first.
        for i in 0..10 {
            c.record(70, 1000 + i * 80);
        }
        // Force past debounce.
        for i in 10..20 {
            c.record(70, 1000 + i * 80);
        }

        // Now go silent for idle_threshold_ms.
        let last_ts = 1000 + 19 * 80;
        c.tick(last_ts + 3000);
        assert_eq!(
            c.state(),
            ProcessState::Idle,
            "idle transition must be instant (no debounce)"
        );
    }

    /// Claude classifier state_ms tracks time correctly.
    #[test]
    fn state_ms_tracking() {
        let mut c = ClaudeClassifier::new(3000, 200);
        c.set_dead(5000);
        assert_eq!(c.state_ms(5500), 500);
    }

    /// state_name covers all known bytes.
    #[test]
    fn state_name_all_values() {
        let c = ClaudeClassifier::new(3000, 200);
        assert_eq!(c.state_name(0x00), "idle");
        assert_eq!(c.state_name(0x01), "thinking");
        assert_eq!(c.state_name(0x02), "streaming");
        assert_eq!(c.state_name(0x03), "tool_use");
        assert_eq!(c.state_name(0x04), "active");
        assert_eq!(c.state_name(0xFF), "dead");
        assert_eq!(c.state_name(0x99), "unknown");
    }

    /// tick() after set_dead() must NOT resurrect the process.
    /// Catches: dead session re-classified as Thinking/Streaming because
    /// the window still has recent events that raw_classify would match.
    #[test]
    fn tick_after_set_dead_does_not_resurrect() {
        let mut c = ClaudeClassifier::new(3000, 200);

        // Build up recent output so raw_classify would return non-Idle.
        for i in 0..15u64 {
            c.record(70, 1000 + i * 80);
        }
        let last_ts = 1000 + 14 * 80;

        // Kill it.
        c.set_dead(last_ts + 10);
        assert_eq!(c.state(), ProcessState::Dead);

        // Tick shortly after — window still has fresh data.
        c.tick(last_ts + 50);
        assert_eq!(
            c.state(),
            ProcessState::Dead,
            "tick must not resurrect a dead process"
        );

        // Tick way later too.
        c.tick(last_ts + 100_000);
        assert_eq!(
            c.state(),
            ProcessState::Dead,
            "dead stays dead regardless of time"
        );
    }

    /// record() after set_dead() must NOT resurrect the process.
    #[test]
    fn record_after_set_dead_does_not_resurrect() {
        let mut c = ClaudeClassifier::new(3000, 200);
        c.set_dead(5000);
        c.record(5000, 5100); // big burst that would normally be ToolUse
        assert_eq!(
            c.state(),
            ProcessState::Dead,
            "new output must not resurrect a dead process"
        );
    }

    /// Debounce pending state is replaced when raw state changes mid-debounce.
    /// Catches: old pending timestamp bleeding into new state, causing early transition.
    #[test]
    fn debounce_pending_replaced_on_different_raw_state() {
        let mut c = ClaudeClassifier::new(3000, 200);
        assert_eq!(c.state(), ProcessState::Idle);

        // First record: raw = Thinking, pending set at t=1000.
        c.record(70, 1000);
        assert_eq!(c.state(), ProcessState::Idle, "debounce holds");

        // Inject a ToolUse burst at t=1050 — raw changes before debounce expires.
        c.record(5000, 1050);
        // Pending should now be ToolUse@1050, NOT ToolUse@1000.
        // If old timestamp leaked, debounce would expire at 1000+200=1200.
        assert_eq!(
            c.state(),
            ProcessState::Idle,
            "still debouncing with fresh pending"
        );

        // At t=1150 (100ms after ToolUse pending was set), debounce NOT yet expired.
        c.tick(1150);
        assert_eq!(
            c.state(),
            ProcessState::Idle,
            "100ms < 200ms debounce, should still be Idle"
        );
    }

    /// Window is bounded at WINDOW_SIZE (20) after many records.
    #[test]
    fn window_bounded_at_capacity() {
        let mut c = ClaudeClassifier::new(3000, 200);
        for i in 0..100u64 {
            c.record(70, 1000 + i * 50);
        }
        assert!(
            c.window.len() <= WINDOW_SIZE,
            "window grew past {WINDOW_SIZE}: {}",
            c.window.len()
        );
    }

    /// Streaming pattern detection.
    #[test]
    fn streaming_on_variable_bursts() {
        let mut c = ClaudeClassifier::new(3000, 0); // 0 debounce for raw testing
        // Variable-size bursts at high frequency -> streaming.
        for i in 0..15u64 {
            let size = 200 + ((i % 5) as usize) * 150; // 200..800 byte range
            c.record(size, 1000 + i * 40);
        }
        let raw = c.raw_classify(1000 + 14 * 40);
        assert_eq!(raw, ProcessState::Streaming);
    }
}
