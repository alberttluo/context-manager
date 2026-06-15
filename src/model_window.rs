use crate::config::Config;

const KNOWN_TIERS: [u64; 2] = [200_000, 1_000_000];

/// Resolve the context window (in tokens) for a model id, falling back to the
/// configured default for unknown or missing models.
///
/// If observed_max_tokens exceeds the configured window, the real window must
/// be larger — bump to the smallest known tier that fits.
pub fn resolve_window(model: Option<&str>, observed_max_tokens: u64, config: &Config) -> u64 {
    let base = model
        .and_then(|m| config.model_windows.overrides.get(m).copied())
        .unwrap_or(config.model_windows.default);
    if observed_max_tokens <= base {
        return base;
    }
    KNOWN_TIERS
        .iter()
        .copied()
        .find(|&t| t >= observed_max_tokens)
        .unwrap_or(observed_max_tokens)
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
        assert_eq!(resolve_window(Some("claude-opus-4-8"), 0, &c), 1_000_000);
    }

    #[test]
    fn falls_back_to_default_for_unknown_model() {
        let c = config_with_override();
        assert_eq!(resolve_window(Some("some-future-model"), 0, &c), 200_000);
    }

    #[test]
    fn falls_back_to_default_for_missing_model() {
        let c = config_with_override();
        assert_eq!(resolve_window(None, 0, &c), 200_000);
    }

    #[test]
    fn empirical_bumps_when_observed_exceeds_default() {
        let c = config_with_override();
        // No override for this model; default is 200k; observed 501_522 -> 1_000_000.
        assert_eq!(resolve_window(Some("some-future-model"), 501_522, &c), 1_000_000);
    }

    #[test]
    fn config_override_still_wins_when_observed_below_it() {
        let c = config_with_override();
        // Override for opus is 1_000_000; observed 501_522 is below that -> 1_000_000.
        assert_eq!(resolve_window(Some("claude-opus-4-8"), 501_522, &c), 1_000_000);
    }

    #[test]
    fn observed_beyond_largest_tier_returns_observed() {
        let c = config_with_override();
        assert_eq!(resolve_window(None, 1_200_000, &c), 1_200_000);
    }
}
