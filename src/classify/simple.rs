//! Simple binary classifier: idle or active.
//!
//! Reports Idle when no output for >= threshold, Active otherwise.
//! No pattern analysis — just a silence detector.

use super::{ProcessState, StateClassifier};

pub struct SimpleClassifier {
    last_output_ms: u64,
    current_state: ProcessState,
    state_entered_at: u64,
    idle_threshold_ms: u64,
}

impl SimpleClassifier {
    pub fn new(idle_threshold_ms: u64) -> Self {
        Self {
            last_output_ms: 0,
            current_state: ProcessState::Idle,
            state_entered_at: 0,
            idle_threshold_ms,
        }
    }

    fn reclassify(&mut self, now_ms: u64) {
        // Dead is terminal — never transition out of it.
        if self.current_state == ProcessState::Dead {
            return;
        }
        let new = if now_ms.saturating_sub(self.last_output_ms) >= self.idle_threshold_ms {
            ProcessState::Idle
        } else {
            ProcessState::Active
        };
        if new != self.current_state {
            self.current_state = new;
            self.state_entered_at = now_ms;
        }
    }
}

impl StateClassifier for SimpleClassifier {
    fn record(&mut self, _byte_count: usize, now_ms: u64) {
        self.last_output_ms = now_ms;
        self.reclassify(now_ms);
    }

    fn tick(&mut self, now_ms: u64) {
        self.reclassify(now_ms);
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
    }

    fn state_name(&self, state: u8) -> &'static str {
        match state {
            0x00 => "idle",
            0x04 => "active",
            0xFF => "dead",
            _ => "unknown",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Issue #6: simple classifier uses Active (0x04), NOT Streaming (0x02).
    #[test]
    fn simple_uses_active_not_streaming() {
        let mut c = SimpleClassifier::new(3000);
        c.record(100, 5000);
        assert_eq!(c.state(), ProcessState::Active);
        assert_ne!(c.state(), ProcessState::Streaming);
    }

    /// Issue #10: record output -> Active, silence -> Idle, set_dead -> Dead.
    #[test]
    fn lifecycle_active_idle_dead() {
        let mut c = SimpleClassifier::new(3000);
        assert_eq!(c.state(), ProcessState::Idle, "starts Idle");

        c.record(50, 1000);
        assert_eq!(c.state(), ProcessState::Active, "output -> Active");

        c.tick(5000);
        assert_eq!(c.state(), ProcessState::Idle, "silence -> Idle");

        c.set_dead(6000);
        assert_eq!(c.state(), ProcessState::Dead, "set_dead -> Dead");
    }

    /// state_ms tracks time in current state.
    #[test]
    fn state_ms_tracking() {
        let mut c = SimpleClassifier::new(3000);
        c.record(100, 1000);
        assert_eq!(c.state_ms(1500), 500);
    }

    /// state_name maps correctly for simple classifier.
    #[test]
    fn state_name_values() {
        let c = SimpleClassifier::new(3000);
        assert_eq!(c.state_name(0x00), "idle");
        assert_eq!(c.state_name(0x04), "active");
        assert_eq!(c.state_name(0xFF), "dead");
        assert_eq!(c.state_name(0x02), "unknown");
    }

    /// Multiple records keep state Active.
    #[test]
    fn multiple_records_stay_active() {
        let mut c = SimpleClassifier::new(3000);
        c.record(10, 1000);
        c.record(20, 2000);
        c.record(30, 2500);
        assert_eq!(c.state(), ProcessState::Active);
    }

    /// tick just under idle threshold stays Active.
    #[test]
    fn tick_under_threshold_stays_active() {
        let mut c = SimpleClassifier::new(3000);
        c.record(100, 1000);
        c.tick(3999); // 2999ms since last output, under 3000
        assert_eq!(c.state(), ProcessState::Active);
    }

    /// set_dead() then tick() with recent output must NOT resurrect.
    /// Catches: reclassify() overriding Dead with Active because last_output_ms is recent.
    #[test]
    fn set_dead_then_tick_stays_dead() {
        let mut c = SimpleClassifier::new(3000);
        c.record(100, 5000); // recent output
        assert_eq!(c.state(), ProcessState::Active);

        c.set_dead(5100);
        assert_eq!(c.state(), ProcessState::Dead);

        // Tick with last_output_ms still "recent" — reclassify would say Active.
        c.tick(5200);
        assert_eq!(
            c.state(),
            ProcessState::Dead,
            "tick must not resurrect a dead process"
        );
    }

    /// record() after set_dead() must NOT resurrect.
    #[test]
    fn record_after_set_dead_stays_dead() {
        let mut c = SimpleClassifier::new(3000);
        c.set_dead(5000);
        c.record(100, 5100);
        assert_eq!(
            c.state(),
            ProcessState::Dead,
            "new output must not resurrect a dead process"
        );
    }

    /// tick exactly at idle threshold transitions to Idle.
    #[test]
    fn tick_at_threshold_goes_idle() {
        let mut c = SimpleClassifier::new(3000);
        c.record(100, 1000);
        c.tick(4000); // exactly 3000ms since last output
        assert_eq!(c.state(), ProcessState::Idle);
    }
}
