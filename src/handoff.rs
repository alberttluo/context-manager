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

/// Quote a single token for safe inclusion in a shell command line.
fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/' | b'=' | b':' | b',' | b'@' | b'+')) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Flags whose value is the following argv token (kept together).
const VALUE_FLAGS: &[&str] = &[
    "--model", "--fallback-model", "--permission-mode", "--permission-prompt-tool",
    "--add-dir", "--mcp-config", "--settings", "--setting-sources",
    "--append-system-prompt", "--allowedTools", "--disallowedTools", "--agents",
    "--output-format", "--input-format", "--session-id",
];
/// Session-continuation flags — dropped, since a handoff starts a FRESH session.
const DROP_FLAGS: &[&str] = &["--continue", "-c", "--resume", "-r"];

/// Keep the original session's option flags (and their values) while dropping
/// any stale positional prompt and session-continuation flags, so the successor
/// inherits the same configuration but receives only our handoff prompt.
fn sanitize_launch_flags(raw: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        let arg = &raw[i];
        if !arg.starts_with('-') {
            i += 1; // bare positional (e.g. a stale initial prompt) — drop it
            continue;
        }
        let name = arg.split('=').next().unwrap_or(arg);
        if DROP_FLAGS.contains(&name) {
            i += 1;
            continue;
        }
        out.push(arg.clone());
        if !arg.contains('=') && VALUE_FLAGS.contains(&name) && i + 1 < raw.len() {
            out.push(raw[i + 1].clone());
            i += 2;
        } else {
            i += 1;
        }
    }
    out
}

/// Build the successor launch command, reusing the original session's flags
/// (e.g. --dangerously-skip-permissions, --model) so it behaves identically,
/// and passing the handoff doc as the initial prompt.
fn seed_command(path: &Path, launch_flags: &[String]) -> String {
    let mut parts = vec!["claude".to_string()];
    parts.extend(launch_flags.iter().map(|f| shell_quote(f)));
    parts.push(shell_quote(&format!(
        "Read the handoff document at {} and continue the work described there.",
        path.display()
    )));
    parts.join(" ")
}

/// Drive the live session to write a handoff doc, wait for it, then relaunch the
/// pane: respawn it into a fresh interactive shell and type the seeded claude
/// command into it. Returns the handoff file path on success.
///
/// Launching via the shell (not `respawn-pane 'claude ...'`) is essential — the
/// shell sources init files so the full environment (certs/proxy/PATH) is
/// present, and the pane survives if claude exits.
///
/// On any failure (timeout, tmux error) the pane is left untouched — we never
/// relaunch unless the handoff file is present and stable.
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

    // Capture the original session's launch flags while it is still alive, so the
    // successor inherits them (best-effort: no flags on failure).
    let launch_flags = sanitize_launch_flags(&tmux.pane_launch_flags(&opts.pane).unwrap_or_default());

    tmux.send_text(&opts.pane, &handoff_prompt(&handoff_path))?;
    tmux.send_enter(&opts.pane)?;

    wait_for_stable_file(&handoff_path, opts.timeout_secs, &mut sleep_fn)?;

    // Relaunch in-place: fresh interactive shell, then type the seeded claude
    // command into it (see perform_handoff/respawn_shell docs for why).
    tmux.respawn_shell(&opts.pane)?;
    sleep_fn(Duration::from_secs(3)); // let the shell finish sourcing init files
    tmux.send_text(&opts.pane, &seed_command(&handoff_path, &launch_flags))?;
    tmux.send_enter(&opts.pane)?;
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
    while waited < timeout_secs {
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
        // The original session's raw argv: real flags + a value flag + a
        // session-continuation flag + a stale positional prompt. The successor
        // must inherit the real flags but NOT --continue or the stale prompt.
        fake.set_launch_flags(vec![
            "--dangerously-skip-permissions".into(),
            "--continue".into(),
            "--model".into(), "opus".into(),
            "Reply with READY then wait.".into(),
        ]);

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
        // Then it respawns the pane into a shell...
        assert!(calls.iter().any(|c| c == "respawn_shell:%5"));
        // ...and types the seeded claude command (the 2nd send_text) into it,
        // carrying the original launch flags.
        let seed = calls.iter().filter(|c| c.starts_with("send_text:%5:")).nth(1).unwrap();
        assert!(seed.contains("claude"));
        assert!(seed.contains("--dangerously-skip-permissions"));
        assert!(seed.contains("--model opus"));
        assert!(seed.contains(expected.to_str().unwrap()));
        // Stale positional prompt and --continue must NOT leak into the successor.
        assert!(!seed.contains("Reply with READY"));
        assert!(!seed.contains("--continue"));
    }

    #[test]
    fn sanitize_keeps_flags_drops_prompt_and_continuation() {
        let raw = vec![
            "--dangerously-skip-permissions".to_string(),
            "--continue".to_string(),
            "--model".to_string(), "opus".to_string(),
            "--resume".to_string(),
            "stale prompt".to_string(),
        ];
        assert_eq!(
            sanitize_launch_flags(&raw),
            vec!["--dangerously-skip-permissions", "--model", "opus"]
        );
        // --flag=value form is kept whole; no value-token is consumed after it.
        assert_eq!(
            sanitize_launch_flags(&["--model=opus".to_string(), "old".to_string()]),
            vec!["--model=opus"]
        );
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
        // It must NOT have relaunched the pane on failure.
        assert!(!fake.calls().iter().any(|c| c.starts_with("respawn_shell:")));
    }
}
