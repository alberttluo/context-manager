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

/// Consecutive failed handoff attempts after which a session is left alone.
///
/// Without a cap, a session that cannot complete a handoff (its pane is wedged,
/// or the model keeps ignoring the prompt) is re-prompted every
/// timeout+cooldown for as long as it stays over threshold, which is what
/// produced the same session being asked to hand off five times.
pub const MAX_HANDOFF_FAILURES: u32 = 3;

pub struct SessionMonitor {
    /// When the current quiet stretch began (reset on any transcript change).
    quiet_since: Instant,
    /// Set when the grace notice has been sent; None otherwise.
    grace_started: Option<Instant>,
    /// Handoffs are suppressed until this time.
    cooldown_until: Option<Instant>,
    consecutive_failures: u32,
    /// Set once the failure cap is hit; no further handoffs are attempted.
    abandoned: bool,
}

impl SessionMonitor {
    pub fn new(now: Instant) -> Self {
        SessionMonitor {
            quiet_since: now,
            grace_started: None,
            cooldown_until: None,
            consecutive_failures: 0,
            abandoned: false,
        }
    }

    pub fn note_handoff_done(&mut self, now: Instant) {
        self.grace_started = None;
        self.consecutive_failures = 0;
        // cooldown is applied relative to the configured duration at tick time;
        // store the anchor and treat cooldown as active while
        // (now - anchor) < cooldown_secs.
        self.cooldown_until = Some(now);
    }

    /// Record a failed attempt. Returns the new consecutive-failure count.
    pub fn note_handoff_failed(&mut self, now: Instant) -> u32 {
        self.grace_started = None;
        self.cooldown_until = Some(now);
        self.consecutive_failures += 1;
        if self.consecutive_failures >= MAX_HANDOFF_FAILURES {
            self.abandoned = true;
        }
        self.consecutive_failures
    }

    /// The user took the session back. Back off without counting a failure —
    /// they are working, and the next quiet period will reconsider.
    pub fn note_superseded(&mut self, now: Instant) {
        self.grace_started = None;
        self.cooldown_until = Some(now);
    }

    pub fn is_abandoned(&self) -> bool {
        self.abandoned
    }

    /// Cooldown lengthens with each consecutive failure (up to 8x) so a session
    /// that keeps failing is retried progressively less often.
    fn effective_cooldown(&self, cooldown_secs: u64) -> u64 {
        cooldown_secs.saturating_mul(1 << self.consecutive_failures.min(3))
    }

    fn cooldown_active(&self, now: Instant, cooldown_secs: u64) -> bool {
        match self.cooldown_until {
            Some(anchor) => {
                now.duration_since(anchor).as_secs() < self.effective_cooldown(cooldown_secs)
            }
            None => false,
        }
    }

    pub fn tick(&mut self, now: Instant, input: &TickInput) -> TickOutcome {
        if self.abandoned {
            return TickOutcome::Idle;
        }
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
    fn abandons_session_after_repeated_failures() {
        let t0 = Instant::now();
        let mut m = SessionMonitor::new(t0);
        for i in 1..MAX_HANDOFF_FAILURES {
            assert_eq!(m.note_handoff_failed(t0), i);
            assert!(!m.is_abandoned(), "should not abandon after {i} failure(s)");
        }
        assert_eq!(m.note_handoff_failed(t0), MAX_HANDOFF_FAILURES);
        assert!(m.is_abandoned());
        // Well past any cooldown, an abandoned session is never re-triggered.
        let later = t0 + Duration::from_secs(100_000);
        assert_eq!(m.tick(later, &over_threshold_idle()), TickOutcome::Idle);
    }

    #[test]
    fn cooldown_lengthens_with_each_failure() {
        let t0 = Instant::now();
        let mut m = SessionMonitor::new(t0);
        m.note_handoff_failed(t0); // 1 failure -> 2x the 120s cooldown
        // 200s later still inside the doubled cooldown.
        let t1 = t0 + Duration::from_secs(200);
        assert_eq!(m.tick(t1, &over_threshold_idle()), TickOutcome::Idle);
        // Past 240s it is eligible again.
        let t2 = t0 + Duration::from_secs(250);
        assert_eq!(m.tick(t2, &over_threshold_idle()), TickOutcome::NotifyGrace);
    }

    #[test]
    fn success_resets_failure_backoff() {
        let t0 = Instant::now();
        let mut m = SessionMonitor::new(t0);
        m.note_handoff_failed(t0);
        m.note_handoff_done(t0);
        // Back to the plain 120s cooldown: eligible again just after it.
        let t1 = t0 + Duration::from_secs(130);
        assert_eq!(m.tick(t1, &over_threshold_idle()), TickOutcome::NotifyGrace);
    }

    #[test]
    fn supersede_backs_off_without_counting_a_failure() {
        let t0 = Instant::now();
        let mut m = SessionMonitor::new(t0);
        m.note_superseded(t0);
        assert!(!m.is_abandoned());
        // Plain cooldown applies (no backoff multiplier).
        let t1 = m_after(t0, 130);
        assert_eq!(m.tick(t1, &over_threshold_idle()), TickOutcome::NotifyGrace);
    }

    fn m_after(t: Instant, secs: u64) -> Instant {
        t + Duration::from_secs(secs)
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
