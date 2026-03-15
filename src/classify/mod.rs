//! Pluggable state classification for pty output patterns.
//!
//! The classifier analyses pty output byte patterns to infer what the
//! supervised process is doing. Different programs have different output
//! signatures, so the classifier is selected at runtime via config.

pub mod claude;
pub mod none;
pub mod simple;

use crate::config::ClassifierConfig;

/// Observable process states derived from pty output patterns.
///
/// This enum is the protocol-level contract — every byte that can appear
/// on the wire as a state value is defined here. Each classifier uses a
/// subset of these states.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ProcessState {
    /// No output for >= idle threshold. Used by all classifiers.
    Idle = 0x00,
    /// Spinner or loading indicator — uniform small bursts. Used by `claude`.
    Thinking = 0x01,
    /// Token streaming — variable bursts at high frequency. Used by `claude`.
    Streaming = 0x02,
    /// Tool execution — large bursts after a pause. Used by `claude`.
    ToolUse = 0x03,
    /// Generic "producing output" — no pattern distinction. Used by `simple`.
    Active = 0x04,
    /// Child process exited. Used by all classifiers.
    Dead = 0xFF,
}

/// A single pty output event recorded by the classifier.
pub struct OutputEvent {
    pub timestamp_ms: u64,
    pub byte_count: usize,
}

/// Trait for state classifiers.
pub trait StateClassifier: Send {
    /// Record a new pty output event.
    fn record(&mut self, byte_count: usize, now_ms: u64);

    /// Re-evaluate state without new output (for idle transitions).
    fn tick(&mut self, now_ms: u64);

    /// Current classified state.
    fn state(&self) -> ProcessState;

    /// Milliseconds in current state.
    fn state_ms(&self, now_ms: u64) -> u32;

    /// Force state to Dead (on child exit).
    fn set_dead(&mut self, now_ms: u64);

    /// Human-readable name for a state byte.
    fn state_name(&self, state: u8) -> &'static str;
}

/// Create a classifier from config.
pub fn from_config(config: &ClassifierConfig) -> Box<dyn StateClassifier> {
    match config {
        ClassifierConfig::Claude { .. } => Box::new(claude::ClaudeClassifier::new(
            config.idle_threshold_ms(),
            config.debounce_ms(),
        )),
        ClassifierConfig::Simple { .. } => {
            Box::new(simple::SimpleClassifier::new(config.idle_threshold_ms()))
        }
        ClassifierConfig::None => Box::new(none::NoneClassifier),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Issue #5: ProcessState repr bytes are the protocol contract — exhaustive check.
    #[test]
    fn process_state_repr_bytes_stable() {
        assert_eq!(ProcessState::Idle as u8, 0x00);
        assert_eq!(ProcessState::Thinking as u8, 0x01);
        assert_eq!(ProcessState::Streaming as u8, 0x02);
        assert_eq!(ProcessState::ToolUse as u8, 0x03);
        assert_eq!(ProcessState::Active as u8, 0x04);
        assert_eq!(ProcessState::Dead as u8, 0xFF);
    }

    /// from_config produces the correct classifier type for each variant.
    #[test]
    fn from_config_claude() {
        let c = from_config(&ClassifierConfig::Claude {
            idle_threshold_ms: 3000,
            debounce_ms: 200,
        });
        assert_eq!(c.state(), ProcessState::Idle);
        assert_eq!(c.state_name(0x01), "thinking"); // only claude knows "thinking"
    }

    #[test]
    fn from_config_simple() {
        let c = from_config(&ClassifierConfig::Simple {
            idle_threshold_ms: 3000,
        });
        assert_eq!(c.state(), ProcessState::Idle);
        assert_eq!(c.state_name(0x04), "active"); // simple knows "active"
        assert_eq!(c.state_name(0x01), "unknown"); // simple doesn't know "thinking"
    }

    #[test]
    fn from_config_none() {
        let c = from_config(&ClassifierConfig::None);
        assert_eq!(c.state(), ProcessState::Idle);
        assert_eq!(c.state_name(0x01), "idle"); // none reports everything as idle
    }

    /// Issue #6: classifier orthogonality — simple uses Active, claude never does.
    #[test]
    fn classifier_orthogonality() {
        let mut simple = from_config(&ClassifierConfig::Simple {
            idle_threshold_ms: 3000,
        });
        simple.record(100, 1000);
        assert_eq!(simple.state(), ProcessState::Active);

        // Claude with output does NOT go to Active — it goes to Thinking or similar.
        let mut claude = from_config(&ClassifierConfig::Claude {
            idle_threshold_ms: 3000,
            debounce_ms: 0,
        });
        claude.record(100, 1000);
        assert_ne!(claude.state(), ProcessState::Active);
    }
}
