use crate::tmux::TmuxControl;
use anyhow::bail;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub struct HandoffOptions {
    pub pane: String,
    pub session_id: String,
    pub handoff_dir: PathBuf,
    pub timeout_secs: u64,
}

pub fn expected_handoff_path(handoff_dir: &Path, session_id: &str) -> PathBuf {
    handoff_dir.join(format!("{session_id}.md"))
}

fn handoff_prompt(path: &Path) -> String {
    format!(
        "Write a complete handoff document to {} using the Write tool. Capture \
the current task, what has been done, key decisions, the current state, and the \
exact next steps, so a fresh session can continue with no prior context. After \
writing the file, reply with exactly: HANDOFF_COMPLETE",
        path.display()
    )
}

fn seed_command(path: &Path) -> String {
    format!(
        "claude \"Read the handoff document at {} and continue the work described there.\"",
        path.display()
    )
}

/// Drive the live session to write a handoff doc, wait for it, then respawn the
/// pane with a fresh seeded session. Returns the handoff file path on success.
///
/// On any failure (timeout, tmux error) the pane is left untouched — we never
/// respawn unless the handoff file is present and stable.
pub fn perform_handoff<S>(
    tmux: &dyn TmuxControl,
    opts: &HandoffOptions,
    mut sleep_fn: S,
) -> anyhow::Result<PathBuf>
where
    S: FnMut(Duration),
{
    std::fs::create_dir_all(&opts.handoff_dir)?;
    let handoff_path = expected_handoff_path(&opts.handoff_dir, &opts.session_id);
    // Stale file from a prior aborted attempt must not be mistaken for success.
    let _ = std::fs::remove_file(&handoff_path);

    tmux.send_text(&opts.pane, &handoff_prompt(&handoff_path))?;
    tmux.send_enter(&opts.pane)?;

    wait_for_stable_file(&handoff_path, opts.timeout_secs, &mut sleep_fn)?;

    tmux.respawn(&opts.pane, &seed_command(&handoff_path))?;
    Ok(handoff_path)
}

/// Poll until the file exists and its size is unchanged across two consecutive
/// polls (so we don't respawn while the model is mid-write), or the timeout
/// elapses.
fn wait_for_stable_file<S>(path: &Path, timeout_secs: u64, sleep_fn: &mut S) -> anyhow::Result<()>
where
    S: FnMut(Duration),
{
    let poll = Duration::from_secs(1);
    let mut waited = 0u64;
    let mut last_size: Option<u64> = None;
    while waited <= timeout_secs {
        if let Ok(meta) = std::fs::metadata(path) {
            let size = meta.len();
            if size > 0 && last_size == Some(size) {
                return Ok(());
            }
            last_size = Some(size);
        }
        sleep_fn(poll);
        waited += 1;
    }
    bail!("handoff file {} did not stabilize within {}s", path.display(), timeout_secs);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::FakeTmux;

    #[test]
    fn drives_session_then_respawns_seeded() {
        let dir = tempfile::tempdir().unwrap();
        let handoff_dir = dir.path().join("handoffs");
        let fake = FakeTmux::new();

        // Pre-create the handoff file so wait returns immediately. In real use
        // the live session writes it; here we simulate that the moment send_text
        // would have triggered it.
        std::fs::create_dir_all(&handoff_dir).unwrap();

        let opts = HandoffOptions {
            pane: "%5".into(),
            session_id: "sess-9".into(),
            handoff_dir: handoff_dir.clone(),
            timeout_secs: 5,
        };

        // sleep_fn that, on first call, writes the expected file so the poll
        // sees it on the next iteration.
        let expected = expected_handoff_path(&handoff_dir, "sess-9");
        let exp2 = expected.clone();
        let sleep_fn = move |_d: std::time::Duration| {
            std::fs::write(&exp2, "handoff body").unwrap();
        };

        let result = perform_handoff(&fake, &opts, sleep_fn).unwrap();
        assert_eq!(result, expected);

        let calls = fake.calls();
        // First it sends the handoff prompt text + Enter.
        assert!(calls[0].starts_with("send_text:%5:"));
        assert!(calls[0].contains(expected.to_str().unwrap()));
        assert_eq!(calls[1], "send_enter:%5");
        // Then it respawns the pane with a claude command that reads the handoff.
        let respawn = calls.iter().find(|c| c.starts_with("respawn:%5:")).unwrap();
        assert!(respawn.contains("claude"));
        assert!(respawn.contains(expected.to_str().unwrap()));
    }

    #[test]
    fn times_out_when_handoff_file_never_appears() {
        let dir = tempfile::tempdir().unwrap();
        let opts = HandoffOptions {
            pane: "%5".into(),
            session_id: "sess-x".into(),
            handoff_dir: dir.path().join("handoffs"),
            timeout_secs: 1,
        };
        let fake = FakeTmux::new();
        let sleep_fn = |_d: std::time::Duration| {};
        let result = perform_handoff(&fake, &opts, sleep_fn);
        assert!(result.is_err());
        // It must NOT have respawned the pane on failure.
        assert!(!fake.calls().iter().any(|c| c.starts_with("respawn:")));
    }
}
