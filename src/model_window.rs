use crate::config::Config;

/// Resolve the context window (in tokens) for a model id, falling back to the
/// configured default for unknown or missing models.
pub fn resolve_window(model: Option<&str>, config: &Config) -> u64 {
    if let Some(m) = model {
        if let Some(w) = config.model_windows.overrides.get(m) {
            return *w;
        }
    }
    config.model_windows.default
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
        assert_eq!(resolve_window(Some("claude-opus-4-8"), &c), 1_000_000);
    }

    #[test]
    fn falls_back_to_default_for_unknown_model() {
        let c = config_with_override();
        assert_eq!(resolve_window(Some("some-future-model"), &c), 200_000);
    }

    #[test]
    fn falls_back_to_default_for_missing_model() {
        let c = config_with_override();
        assert_eq!(resolve_window(None, &c), 200_000);
    }
}
