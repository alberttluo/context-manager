use anyhow::Context;
use context_manager::paths::Paths;
use context_manager::registration::{self, Registration};
use serde::Deserialize;
use std::io::Read;

#[derive(Deserialize)]
struct HookInput {
    session_id: String,
    #[serde(default)]
    transcript_path: String,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    hook_event_name: String,
}

fn sessions_dir() -> anyhow::Result<std::path::PathBuf> {
    if let Ok(dir) = std::env::var("CONTEXT_MANAGER_SESSIONS_DIR") {
        return Ok(dir.into());
    }
    Ok(Paths::resolve()?.sessions_dir())
}

fn main() -> anyhow::Result<()> {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf).context("reading hook stdin")?;
    let input: HookInput = serde_json::from_str(&buf).context("parsing hook JSON")?;

    let dir = sessions_dir()?;

    if input.hook_event_name.eq_ignore_ascii_case("SessionEnd") {
        registration::remove(&dir, &input.session_id)?;
        return Ok(());
    } else if !input.hook_event_name.eq_ignore_ascii_case("SessionStart") {
        return Ok(());
    }

    // Only register sessions running inside tmux — we can only act on those.
    let tmux_pane = match std::env::var("TMUX_PANE") {
        Ok(p) if !p.is_empty() => p,
        _ => return Ok(()), // not in tmux: nothing to manage, exit quietly
    };

    let reg = Registration {
        session_id: input.session_id,
        transcript_path: input.transcript_path.into(),
        cwd: input.cwd.into(),
        tmux_pane,
        pid: std::process::id(),
        started_at: chrono::Utc::now().to_rfc3339(),
    };
    registration::write(&dir, &reg)?;
    Ok(())
}
