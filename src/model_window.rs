use crate::config::Config;

const KNOWN_TIERS: [u64; 2] = [200_000, 1_000_000];

/// Where a window estimate came from, so a caller can tell a known window from
/// a guess.
///
/// This distinction exists because guessing low is expensive and silent. A model
/// id carries no indication of its window — Claude Code records the 1M Opus
/// variant as plain "claude-opus-5", the same id a 200k session reports — so an
/// unlisted model falling back to a 200k default hands a 1M session off at 45%
/// of 200k, i.e. 9% of the context it actually has. That went unnoticed for a
/// week because nothing said the window was a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowSource {
    /// The model id has an explicit entry in [model_windows]. Trustworthy.
    Configured,
    /// Usage was observed above the fallback, which proves the window is at
    /// least that large; raised to the smallest known tier that fits.
    Observed,
    /// Neither — the configured default, applied to a model nobody listed.
    Default,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowEstimate {
    pub tokens: u64,
    pub source: WindowSource,
}

impl WindowEstimate {
    /// True when the window is an unverified fallback rather than something
    /// configured or observed.
    pub fn is_guess(&self) -> bool {
        self.source == WindowSource::Default
    }
}

/// Resolve the context window (in tokens) for a model id, falling back to the
/// configured default for unknown or missing models.
///
/// If observed_max_tokens exceeds the configured window, the real window must
/// be larger — bump to the smallest known tier that fits.
pub fn resolve_window(
    model: Option<&str>,
    observed_max_tokens: u64,
    config: &Config,
) -> WindowEstimate {
    let configured = model.and_then(|m| config.model_windows.overrides.get(m).copied());
    let base = configured.unwrap_or(config.model_windows.default);
    if observed_max_tokens <= base {
        let source = if configured.is_some() {
            WindowSource::Configured
        } else {
            WindowSource::Default
        };
        return WindowEstimate { tokens: base, source };
    }
    let tokens = KNOWN_TIERS
        .iter()
        .copied()
        .find(|&t| t >= observed_max_tokens)
        .unwrap_or(observed_max_tokens);
    WindowEstimate { tokens, source: WindowSource::Observed }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn config_with_override() -> Config {
        let toml_str = r#"
            [model_windows]
            default = 200000
            "claude-opus-4-8" = 1000000
        "#;
        Config::from_toml_str(toml_str).unwrap()
    }

    #[test]
    fn uses_override_when_present() {
        let c = config_with_override();
        let w = resolve_window(Some("claude-opus-4-8"), 0, &c);
        assert_eq!(w.tokens, 1_000_000);
        assert_eq!(w.source, WindowSource::Configured);
        assert!(!w.is_guess());
    }

    #[test]
    fn falls_back_to_default_for_unknown_model() {
        let c = config_with_override();
        let w = resolve_window(Some("some-future-model"), 0, &c);
        assert_eq!(w.tokens, 200_000);
        // The point of the flag: this number is a guess, and the caller must be
        // able to say so rather than presenting it as fact.
        assert_eq!(w.source, WindowSource::Default);
        assert!(w.is_guess());
    }

    #[test]
    fn falls_back_to_default_for_missing_model() {
        let c = config_with_override();
        let w = resolve_window(None, 0, &c);
        assert_eq!(w.tokens, 200_000);
        assert!(w.is_guess());
    }

    #[test]
    fn empirical_bumps_when_observed_exceeds_default() {
        let c = config_with_override();
        // No override for this model; default is 200k; observed 501_522 -> 1_000_000.
        let w = resolve_window(Some("some-future-model"), 501_522, &c);
        assert_eq!(w.tokens, 1_000_000);
        // Observation is evidence, not a guess — no warning is warranted.
        assert_eq!(w.source, WindowSource::Observed);
        assert!(!w.is_guess());
    }

    #[test]
    fn config_override_still_wins_when_observed_below_it() {
        let c = config_with_override();
        // Override for opus is 1_000_000; observed 501_522 is below that -> 1_000_000.
        let w = resolve_window(Some("claude-opus-4-8"), 501_522, &c);
        assert_eq!(w.tokens, 1_000_000);
        assert_eq!(w.source, WindowSource::Configured);
    }

    #[test]
    fn observed_beyond_largest_tier_returns_observed() {
        let c = config_with_override();
        let w = resolve_window(None, 1_200_000, &c);
        assert_eq!(w.tokens, 1_200_000);
        assert_eq!(w.source, WindowSource::Observed);
    }
}
