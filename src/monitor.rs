use crate::decision::{eligible_for_handoff, EligibilityInputs};
use crate::transcript::EntryKind;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickOutcome {
    Idle,
    NotifyGrace,
    ExecuteHandoff,
    CancelGrace,
}

pub struct TickInput {
    pub context_pct: f64,
    pub threshold: f64,
    pub last_entry: EntryKind,
    /// True if the transcript was written since the previous tick.
    pub transcript_changed: bool,
    pub quiet_period_secs: u64,
    pub grace_secs: u64,
    pub cooldown_secs: u64,
}

pub struct SessionMonitor {
    /// When the current quiet stretch began (reset on any transcript change).
    quiet_since: Instant,
    /// Set when the grace notice has been sent; None otherwise.
    grace_started: Option<Instant>,
    /// Handoffs are suppressed until this time.
    cooldown_until: Option<Instant>,
}

impl SessionMonitor {
    pub fn new(now: Instant) -> Self {
        SessionMonitor { quiet_since: now, grace_started: None, cooldown_until: None }
    }

    pub fn note_handoff_done(&mut self, now: Instant) {
        self.grace_started = None;
        // cooldown is applied relative to the configured duration at tick time;
        // store the moment so tick() can compare. We mark a sentinel here and
        // let the next tick compute the deadline; simpler: store now and treat
        // cooldown as active while (now - cooldown_anchor) < cooldown_secs.
        self.cooldown_until = Some(now);
    }

    fn cooldown_active(&self, now: Instant, cooldown_secs: u64) -> bool {
        match self.cooldown_until {
            Some(anchor) => now.duration_since(anchor).as_secs() < cooldown_secs,
            None => false,
        }
    }

    pub fn tick(&mut self, now: Instant, input: &TickInput) -> TickOutcome {
        if input.transcript_changed {
            self.quiet_since = now;
            if self.grace_started.take().is_some() {
                return TickOutcome::CancelGrace;
            }
            return TickOutcome::Idle;
        }

        let quiet_elapsed_secs = now.duration_since(self.quiet_since).as_secs();
        let cooldown_active = self.cooldown_active(now, input.cooldown_secs);

        // Already counting down a grace window?
        if let Some(started) = self.grace_started {
            if now.duration_since(started).as_secs() >= input.grace_secs {
                return TickOutcome::ExecuteHandoff;
            }
            return TickOutcome::Idle;
        }

        let eligible = eligible_for_handoff(&EligibilityInputs {
            context_pct: input.context_pct,
            threshold: input.threshold,
            last_entry: input.last_entry,
            quiet_elapsed_secs,
            quiet_period_secs: input.quiet_period_secs,
            cooldown_active,
        });

        if eligible {
            self.grace_started = Some(now);
            TickOutcome::NotifyGrace
        } else {
            TickOutcome::Idle
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::EntryKind;
    use std::time::{Duration, Instant};

    fn over_threshold_idle() -> TickInput {
        TickInput {
            context_pct: 0.60,
            threshold: 0.50,
            last_entry: EntryKind::Assistant,
            transcript_changed: false,
            quiet_period_secs: 45,
            grace_secs: 10,
            cooldown_secs: 120,
        }
    }

    #[test]
    fn activity_resets_quiet_clock_and_holds() {
        let t0 = Instant::now();
        let mut m = SessionMonitor::new(t0);
        // Active turn: transcript changing, not eligible yet.
        let mut input = over_threshold_idle();
        input.transcript_changed = true;
        assert_eq!(m.tick(t0, &input), TickOutcome::Idle);
    }

    #[test]
    fn begins_grace_after_quiet_period() {
        let t0 = Instant::now();
        let mut m = SessionMonitor::new(t0);
        // First observe quiescence start at t0 (no change).
        assert_eq!(m.tick(t0, &over_threshold_idle()), TickOutcome::Idle);
        // 50s later, still quiet and over threshold -> begin grace.
        let t1 = t0 + Duration::from_secs(50);
        assert_eq!(m.tick(t1, &over_threshold_idle()), TickOutcome::NotifyGrace);
    }

    #[test]
    fn executes_after_grace_elapses() {
        let t0 = Instant::now();
        let mut m = SessionMonitor::new(t0);
        m.tick(t0, &over_threshold_idle());
        let t1 = t0 + Duration::from_secs(50);
        assert_eq!(m.tick(t1, &over_threshold_idle()), TickOutcome::NotifyGrace);
        // 11s after grace began -> execute.
        let t2 = t1 + Duration::from_secs(11);
        assert_eq!(m.tick(t2, &over_threshold_idle()), TickOutcome::ExecuteHandoff);
    }

    #[test]
    fn activity_during_grace_cancels() {
        let t0 = Instant::now();
        let mut m = SessionMonitor::new(t0);
        m.tick(t0, &over_threshold_idle());
        let t1 = t0 + Duration::from_secs(50);
        assert_eq!(m.tick(t1, &over_threshold_idle()), TickOutcome::NotifyGrace);
        // User typed: transcript changed during grace -> cancel.
        let t2 = t1 + Duration::from_secs(2);
        let mut input = over_threshold_idle();
        input.transcript_changed = true;
        assert_eq!(m.tick(t2, &input), TickOutcome::CancelGrace);
    }

    #[test]
    fn cooldown_blocks_re_trigger() {
        let t0 = Instant::now();
        let mut m = SessionMonitor::new(t0);
        m.note_handoff_done(t0);
        let t1 = t0 + Duration::from_secs(50);
        // Quiet + over threshold but in cooldown -> Idle.
        assert_eq!(m.tick(t1, &over_threshold_idle()), TickOutcome::Idle);
    }
}
