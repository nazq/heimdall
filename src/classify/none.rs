//! Null classifier — always reports Idle.
//!
//! Use when state classification is not needed and you only want
//! the pty supervision, scrollback, and socket IPC.

use super::{ProcessState, StateClassifier};

pub struct NoneClassifier;

impl StateClassifier for NoneClassifier {
    fn record(&mut self, _byte_count: usize, _now_ms: u64) {}
    fn tick(&mut self, _now_ms: u64) {}

    fn state(&self) -> ProcessState {
        ProcessState::Idle
    }

    fn state_ms(&self, _now_ms: u64) -> u32 {
        0
    }

    fn set_dead(&mut self, _now_ms: u64) {}

    fn state_name(&self, state: u8) -> &'static str {
        match state {
            0xFF => "dead",
            _ => "idle",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Issue #6: none classifier always reports Idle.
    #[test]
    fn always_idle() {
        let mut c = NoneClassifier;
        assert_eq!(c.state(), ProcessState::Idle);

        c.record(1000, 5000);
        assert_eq!(c.state(), ProcessState::Idle);

        c.tick(99999);
        assert_eq!(c.state(), ProcessState::Idle);
    }

    /// state_ms always returns 0 for none classifier.
    #[test]
    fn state_ms_always_zero() {
        let c = NoneClassifier;
        assert_eq!(c.state_ms(99999), 0);
    }

    /// set_dead is a design-intentional no-op: state() still returns Idle.
    /// This means the wire protocol will report Idle even when the child is dead.
    /// Callers must use the `alive` byte in STATUS_RESP, not the state byte.
    #[test]
    fn set_dead_still_returns_idle_not_dead() {
        let mut c = NoneClassifier;
        c.set_dead(5000);
        // NoneClassifier has no mutable state — set_dead is truly a no-op.
        assert_eq!(
            c.state(),
            ProcessState::Idle,
            "none classifier always reports Idle, even after set_dead"
        );
        // This is correct: clients should use the `alive` byte, not the state byte.
    }

    /// state_name returns "idle" for all non-dead states, "dead" for 0xFF.
    #[test]
    fn state_name_values() {
        let c = NoneClassifier;
        assert_eq!(c.state_name(0x00), "idle");
        assert_eq!(c.state_name(0x01), "idle");
        assert_eq!(c.state_name(0x04), "idle");
        assert_eq!(c.state_name(0xFF), "dead");
    }
}
