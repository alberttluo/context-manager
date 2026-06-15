use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Registration {
    pub session_id: String,
    pub transcript_path: PathBuf,
    pub cwd: PathBuf,
    pub tmux_pane: String,
    pub pid: u32,
    pub started_at: String,
}

fn reg_path(dir: &Path, session_id: &str) -> PathBuf {
    dir.join(format!("{session_id}.json"))
}

pub fn write(dir: &Path, reg: &Registration) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let json = serde_json::to_string_pretty(reg)?;
    std::fs::write(reg_path(dir, &reg.session_id), json)?;
    Ok(())
}

pub fn remove(dir: &Path, session_id: &str) -> anyhow::Result<()> {
    let p = reg_path(dir, session_id);
    if p.exists() {
        std::fs::remove_file(p)?;
    }
    Ok(())
}

/// Read every `*.json` registration in the dir. Malformed files are skipped,
/// not fatal — a half-written file from the hook must never crash the daemon.
pub fn scan(dir: &Path) -> anyhow::Result<Vec<Registration>> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e.into()),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        if let Ok(reg) = serde_json::from_str::<Registration>(&text) {
            out.push(reg);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_scan_remove_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let reg = Registration {
            session_id: "sess-1".into(),
            transcript_path: "/home/u/.claude/projects/p/sess-1.jsonl".into(),
            cwd: "/home/u/proj".into(),
            tmux_pane: "%3".into(),
            pid: 4242,
            started_at: "2026-06-15T12:00:00Z".into(),
        };
        write(dir.path(), &reg).unwrap();

        let found = scan(dir.path()).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].session_id, "sess-1");
        assert_eq!(found[0].tmux_pane, "%3");

        remove(dir.path(), "sess-1").unwrap();
        assert_eq!(scan(dir.path()).unwrap().len(), 0);
    }

    #[test]
    fn scan_skips_malformed_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("garbage.json"), "not json").unwrap();
        // scan must not error on a bad file; it skips it.
        assert_eq!(scan(dir.path()).unwrap().len(), 0);
    }

    #[test]
    fn scan_missing_dir_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        assert_eq!(scan(&missing).unwrap().len(), 0);
    }
}
