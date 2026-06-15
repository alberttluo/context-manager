use crate::transcript::EntryKind;

pub struct EligibilityInputs {
    pub context_pct: f64,
    pub threshold: f64,
    pub last_entry: EntryKind,
    pub quiet_elapsed_secs: u64,
    pub quiet_period_secs: u64,
    pub cooldown_active: bool,
}

/// A session is eligible to begin the handoff flow only when it is over
/// threshold, sitting at a completed assistant turn, has been quiet long
/// enough, and is not in post-handoff cooldown.
pub fn eligible_for_handoff(i: &EligibilityInputs) -> bool {
    i.context_pct >= i.threshold
        && i.last_entry == EntryKind::Assistant
        && i.quiet_elapsed_secs >= i.quiet_period_secs
        && !i.cooldown_active
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::EntryKind;

    fn inputs() -> EligibilityInputs {
        EligibilityInputs {
            context_pct: 0.60,
            threshold: 0.50,
            last_entry: EntryKind::Assistant,
            quiet_elapsed_secs: 60,
            quiet_period_secs: 45,
            cooldown_active: false,
        }
    }

    #[test]
    fn eligible_when_all_conditions_met() {
        assert!(eligible_for_handoff(&inputs()));
    }

    #[test]
    fn not_eligible_below_threshold() {
        let mut i = inputs();
        i.context_pct = 0.49;
        assert!(!eligible_for_handoff(&i));
    }

    #[test]
    fn not_eligible_when_last_entry_not_assistant() {
        let mut i = inputs();
        i.last_entry = EntryKind::User;
        assert!(!eligible_for_handoff(&i));
    }

    #[test]
    fn not_eligible_when_not_quiet_long_enough() {
        let mut i = inputs();
        i.quiet_elapsed_secs = 10;
        assert!(!eligible_for_handoff(&i));
    }

    #[test]
    fn not_eligible_during_cooldown() {
        let mut i = inputs();
        i.cooldown_active = true;
        assert!(!eligible_for_handoff(&i));
    }
}
