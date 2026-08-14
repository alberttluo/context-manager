use anyhow::Context;
use std::path::{Path, PathBuf};

pub struct Paths {
    config_dir: PathBuf,
    state_dir: PathBuf,
}

impl Paths {
    pub fn with_base(config_dir: PathBuf, state_dir: PathBuf) -> Self {
        Paths { config_dir, state_dir }
    }

    /// Resolve the XDG dirs, on every platform:
    ///   config: $XDG_CONFIG_HOME or ~/.config      , + /context-manager
    ///   state:  $XDG_DATA_HOME   or ~/.local/share , + /context-manager
    ///
    /// macOS's own convention (~/Library/Application Support) is deliberately
    /// not followed. The installer, the docs and the daemon must name the same
    /// directory or the daemon silently runs on defaults while the user edits a
    /// config it never reads; one rule for all platforms is what guarantees
    /// that. Users who want the native location can still point XDG_CONFIG_HOME
    /// at it.
    pub fn resolve() -> anyhow::Result<Self> {
        let base = directories::BaseDirs::new().context("cannot determine home directory")?;
        Ok(Self::resolve_from(
            base.home_dir(),
            std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
            std::env::var_os("XDG_DATA_HOME").map(PathBuf::from),
        ))
    }

    fn resolve_from(home: &Path, config_home: Option<PathBuf>, data_home: Option<PathBuf>) -> Self {
        // The XDG spec says a relative (or empty) value is invalid and must be
        // ignored rather than resolved against the cwd — a daemon's cwd is not
        // somewhere state should land.
        let absolute_or = |var: Option<PathBuf>, fallback: &str| {
            var.filter(|p| p.is_absolute()).unwrap_or_else(|| home.join(fallback))
        };
        Paths {
            config_dir: absolute_or(config_home, ".config").join("context-manager"),
            state_dir: absolute_or(data_home, ".local/share").join("context-manager"),
        }
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.state_dir.join("sessions")
    }

    pub fn handoff_dir(&self) -> PathBuf {
        self.state_dir.join("handoffs")
    }

    pub fn lineage_file(&self) -> PathBuf {
        self.state_dir.join("lineage.jsonl")
    }
}

impl AsRef<Path> for Paths {
    fn as_ref(&self) -> &Path {
        &self.state_dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_subpaths_from_a_base() {
        let p = Paths::with_base(
            "/home/u/.config/context-manager".into(),
            "/home/u/.local/share/context-manager".into(),
        );
        assert_eq!(p.config_file(), PathBuf::from("/home/u/.config/context-manager/config.toml"));
        assert_eq!(p.sessions_dir(), PathBuf::from("/home/u/.local/share/context-manager/sessions"));
        assert_eq!(p.handoff_dir(), PathBuf::from("/home/u/.local/share/context-manager/handoffs"));
        assert_eq!(p.lineage_file(), PathBuf::from("/home/u/.local/share/context-manager/lineage.jsonl"));
    }

    #[test]
    fn resolves_the_same_dirs_on_every_platform() {
        let p = Paths::resolve_from(Path::new("/Users/u"), None, None);
        assert_eq!(p.config_file(), PathBuf::from("/Users/u/.config/context-manager/config.toml"));
        assert_eq!(p.sessions_dir(), PathBuf::from("/Users/u/.local/share/context-manager/sessions"));
    }

    #[test]
    fn xdg_overrides_win_but_only_when_absolute() {
        let p = Paths::resolve_from(
            Path::new("/Users/u"),
            Some("/elsewhere/config".into()),
            Some("relative/data".into()),
        );
        assert_eq!(p.config_file(), PathBuf::from("/elsewhere/config/context-manager/config.toml"));
        assert_eq!(
            p.sessions_dir(),
            PathBuf::from("/Users/u/.local/share/context-manager/sessions"),
            "a relative XDG_DATA_HOME must be ignored, not resolved against the cwd",
        );
    }
}
