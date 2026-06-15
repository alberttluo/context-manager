use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub threshold: f64,
    pub quiet_period_secs: u64,
    pub grace_secs: u64,
    pub cooldown_secs: u64,
    pub poll_interval_secs: u64,
    pub handoff_timeout_secs: u64,
    pub dry_run: bool,
    pub model_windows: ModelWindows,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ModelWindows {
    pub default: u64,
    #[serde(flatten)]
    pub overrides: HashMap<String, u64>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            threshold: 0.50,
            quiet_period_secs: 45,
            grace_secs: 10,
            cooldown_secs: 120,
            poll_interval_secs: 3,
            handoff_timeout_secs: 180,
            dry_run: false,
            model_windows: ModelWindows::default(),
        }
    }
}

impl Default for ModelWindows {
    fn default() -> Self {
        ModelWindows { default: 200_000, overrides: HashMap::new() }
    }
}

impl Config {
    pub fn from_toml_str(s: &str) -> anyhow::Result<Config> {
        let mut cfg: Config = toml::from_str(s)?;
        cfg.model_windows.overrides.remove("default");
        Ok(cfg)
    }

    pub fn load(path: &Path) -> anyhow::Result<Config> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => return Err(e.into()),
        };
        Config::from_toml_str(&text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let c = Config::default();
        assert_eq!(c.threshold, 0.50);
        assert_eq!(c.quiet_period_secs, 45);
        assert_eq!(c.grace_secs, 10);
        assert_eq!(c.cooldown_secs, 120);
        assert_eq!(c.poll_interval_secs, 3);
        assert_eq!(c.handoff_timeout_secs, 180);
        assert!(!c.dry_run);
        assert_eq!(c.model_windows.default, 200_000);
    }

    #[test]
    fn loads_partial_toml_and_fills_defaults() {
        let toml_str = r#"
            threshold = 0.40
            dry_run = true
            [model_windows]
            default = 1000000
            "claude-opus-4-8" = 200000
        "#;
        let c = Config::from_toml_str(toml_str).unwrap();
        assert_eq!(c.threshold, 0.40);
        assert!(c.dry_run);
        assert_eq!(c.quiet_period_secs, 45); // default preserved
        assert_eq!(c.model_windows.default, 1_000_000);
        assert_eq!(c.model_windows.overrides.get("claude-opus-4-8"), Some(&200_000));
    }
}
