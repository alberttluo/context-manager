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

    /// Resolve from the platform's XDG dirs:
    ///   config: ~/.config/context-manager
    ///   state:  ~/.local/share/context-manager
    pub fn resolve() -> anyhow::Result<Self> {
        let proj = directories::ProjectDirs::from("", "", "context-manager")
            .context("cannot determine home directory")?;
        Ok(Paths {
            config_dir: proj.config_dir().to_path_buf(),
            state_dir: proj.data_dir().to_path_buf(),
        })
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
}
