use crate::transcript::{contains_ignoring_whitespace, prompt_stats};
use crate::tmux::{is_claude_command, TmuxControl};
use anyhow::bail;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How many times to re-type a prompt that never appeared in the input box.
const PROMPT_SEND_ATTEMPTS: u32 = 3;
/// Budget for typed text to show up in the pane's input box.
const DELIVERY_CONFIRM_SECS: u64 = 5;
/// Budget for a submitted prompt to be recorded in the transcript.
const SUBMIT_CONFIRM_SECS: u64 = 15;
/// Budget for the pane's shell to come up after `respawn-pane`.
const SHELL_READY_SECS: u64 = 30;
/// Budget for the successor `claude` process to appear after its command runs.
const SUCCESSOR_READY_SECS: u64 = 60;
/// How many times to re-type the successor command line.
const SUCCESSOR_SEND_ATTEMPTS: u32 = 3;

/// Foreground commands that mean "a shell is waiting at a prompt".
const SHELL_COMMANDS: &[&str] = &["zsh", "bash", "sh", "fish", "dash", "ksh"];

pub struct HandoffOptions {
    pub pane: String,
    pub session_id: String,
    pub handoff_dir: PathBuf,
    pub timeout_secs: u64,
    /// The live session's transcript, used to confirm our prompt was accepted
    /// and to notice the human taking the session back.
    pub transcript_path: PathBuf,
}

#[derive(Debug)]
pub enum HandoffOutcome {
    /// Doc written and the successor session is up in the pane.
    Completed(PathBuf),
    /// The human submitted their own prompt after ours; we stood down without
    /// touching the pane. Not a failure — retrying immediately would fight them.
    Superseded,
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

/// Poll `cond` once a second (checking immediately first) until it holds or
/// `timeout_secs` elapses. Returns whether it held.
fn poll_until<F, S>(timeout_secs: u64, sleep_fn: &mut S, mut cond: F) -> bool
where
    F: FnMut() -> bool,
    S: FnMut(Duration),
{
    for _ in 0..timeout_secs {
        if cond() {
            return true;
        }
        sleep_fn(Duration::from_secs(1));
    }
    cond()
}

/// A row of box-drawing characters, which the TUI uses to frame its input area.
fn is_border_row(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.chars().count() >= 8 && trimmed.chars().all(|c| c == '─')
}

/// The text currently inside Claude Code's framed input area, or None when the
/// pane shows no such frame (e.g. it is a plain shell prompt).
///
/// Scoping to the frame matters: the whole capture also contains the scrollback,
/// where every previously submitted prompt is echoed, so a naive search over the
/// capture cannot tell "waiting in the input box" from "already sent".
fn input_box_text(capture: &str) -> Option<String> {
    let lines: Vec<&str> = capture.lines().collect();
    let borders: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| is_border_row(l))
        .map(|(i, _)| i)
        .collect();
    if borders.len() < 2 {
        return None;
    }
    let (top, bottom) = (borders[borders.len() - 2], borders[borders.len() - 1]);
    let body = lines[top + 1..bottom]
        .iter()
        .map(|l| l.trim_start().trim_start_matches('❯').trim_start())
        .collect::<Vec<_>>()
        .join(" ");
    Some(body.trim().to_string())
}

/// Whether `text` is currently sitting on the pane's input line.
///
/// Matched whitespace-insensitively because the TUI wraps a long prompt across
/// several rows, which breaks a naive substring search.
fn input_box_holds(tmux: &dyn TmuxControl, pane: &str, text: &str) -> bool {
    match tmux.capture_pane(pane) {
        // No frame means a plain shell, where the typed line is the whole capture.
        Ok(cap) => {
            let scope = input_box_text(&cap).unwrap_or(cap);
            contains_ignoring_whitespace(&scope, text)
        }
        Err(_) => false,
    }
}

/// Whether the user has unsent text sitting in the pane's input box.
///
/// Text typed while the model is mid-turn is queued by the TUI and is NOT
/// written to the transcript until it is processed, so transcript-based takeover
/// detection cannot see it. Starting a handoff anyway would both clear the box
/// (destroying what they typed) and retire the session out from under them.
pub fn user_input_pending(tmux: &dyn TmuxControl, pane: &str) -> bool {
    match tmux.capture_pane(pane) {
        Ok(cap) => input_box_text(&cap).map(|t| !t.is_empty()).unwrap_or(false),
        Err(_) => false,
    }
}

/// Type a prompt into a live Claude session and confirm the session actually
/// received it, retrying if not.
///
/// Claude Code's TUI silently discards keystrokes that arrive before it is ready
/// to read input — verified empirically: text sent one second after the `claude`
/// process appears vanishes with no trace, leaving an empty input box. A blind
/// send-then-Enter therefore loses the prompt entirely, and the old code then sat
/// out its full file-wait timeout for a prompt the session had never seen, before
/// re-firing the whole handoff. So: confirm the text landed in the input box
/// before submitting, and confirm the transcript recorded it after.
fn send_prompt_verified<S>(
    tmux: &dyn TmuxControl,
    pane: &str,
    transcript: &Path,
    text: &str,
    marker: &str,
    sleep_fn: &mut S,
) -> anyhow::Result<()>
where
    S: FnMut(Duration),
{
    let baseline = prompt_stats(transcript, marker)?.with_marker;
    for attempt in 1..=PROMPT_SEND_ATTEMPTS {
        // Drop any partial text a previous attempt may have left behind, so
        // retries never concatenate into a corrupted prompt.
        tmux.clear_input(pane)?;
        tmux.send_text(pane, text)?;

        if !poll_until(DELIVERY_CONFIRM_SECS, sleep_fn, || input_box_holds(tmux, pane, text)) {
            eprintln!("[cm] prompt did not reach pane {pane} input box (attempt {attempt}/{PROMPT_SEND_ATTEMPTS})");
            continue;
        }

        tmux.send_enter(pane)?;
        let submitted = poll_until(SUBMIT_CONFIRM_SECS, sleep_fn, || {
            prompt_stats(transcript, marker).map(|s| s.with_marker > baseline).unwrap_or(false)
        });
        if submitted {
            return Ok(());
        }
        eprintln!("[cm] prompt reached pane {pane} but was never submitted (attempt {attempt}/{PROMPT_SEND_ATTEMPTS})");
    }
    bail!("could not deliver handoff prompt to pane {pane} after {PROMPT_SEND_ATTEMPTS} attempts")
}

/// Wait for the handoff doc to appear and stop growing, giving up early if the
/// human takes the session back.
///
/// Returns `Ok(true)` when the file is ready, `Ok(false)` when superseded.
fn await_handoff_doc<S>(
    path: &Path,
    transcript: &Path,
    marker: &str,
    baseline_prompts: usize,
    timeout_secs: u64,
    sleep_fn: &mut S,
) -> anyhow::Result<bool>
where
    S: FnMut(Duration),
{
    let mut last_size: Option<u64> = None;
    for _ in 0..timeout_secs {
        if let Ok(meta) = std::fs::metadata(path) {
            let size = meta.len();
            if size > 0 && last_size == Some(size) {
                return Ok(true);
            }
            last_size = Some(size);
        }
        // One extra prompt beyond the baseline is our own injected one; anything
        // further means the human is driving the session again.
        if let Ok(stats) = prompt_stats(transcript, marker) {
            if stats.prompts > baseline_prompts + 1 {
                return Ok(false);
            }
        }
        sleep_fn(Duration::from_secs(1));
    }
    bail!("handoff file {} did not stabilize within {}s", path.display(), timeout_secs);
}

fn is_shell(cmd: &str) -> bool {
    SHELL_COMMANDS.contains(&cmd)
}

/// Replace the retired session with a fresh one seeded from the handoff doc.
///
/// The pane is respawned into an interactive shell and the `claude` command is
/// typed into it — launching via the shell (rather than `respawn-pane 'claude
/// ...'`, which runs under a bare `sh -c` that skips shell init) is what makes
/// certs/proxy/PATH available, and it leaves the pane on a shell prompt instead
/// of destroying it if claude later exits.
///
/// Every step is verified. The previous version slept a flat 3 seconds and then
/// typed blind, which raced shell startup: on a loaded machine (or a cwd on a
/// slow mount) the command line was dropped and the pane was left sitting at a
/// bare shell with the old session already killed — the observed "handoff
/// reported success but the session never switched".
fn launch_successor<S>(
    tmux: &dyn TmuxControl,
    pane: &str,
    command: &str,
    sleep_fn: &mut S,
) -> anyhow::Result<()>
where
    S: FnMut(Duration),
{
    tmux.respawn_shell(pane)?;

    if !poll_until(SHELL_READY_SECS, sleep_fn, || {
        tmux.pane_command(pane).map(|c| is_shell(&c)).unwrap_or(false)
    }) {
        bail!("pane {pane} never returned to a shell prompt within {SHELL_READY_SECS}s");
    }

    for attempt in 1..=SUCCESSOR_SEND_ATTEMPTS {
        tmux.clear_input(pane)?;
        tmux.send_text(pane, command)?;
        if !poll_until(DELIVERY_CONFIRM_SECS, sleep_fn, || input_box_holds(tmux, pane, command)) {
            eprintln!("[cm] successor command did not reach pane {pane} (attempt {attempt}/{SUCCESSOR_SEND_ATTEMPTS})");
            continue;
        }
        tmux.send_enter(pane)?;
        if poll_until(SUCCESSOR_READY_SECS, sleep_fn, || {
            tmux.pane_command(pane).map(|c| is_claude_command(&c)).unwrap_or(false)
        }) {
            return Ok(());
        }
        eprintln!("[cm] successor claude did not start in pane {pane} (attempt {attempt}/{SUCCESSOR_SEND_ATTEMPTS})");
    }

    // The old session is already gone, so make the recovery obvious rather than
    // leaving a silently dead pane.
    let _ = tmux.display_message(pane, "[context-manager] successor failed to start — run the claude command in this pane");
    bail!("successor claude never started in pane {pane}")
}

/// Drive the live session to write a handoff doc, then replace it in-place with a
/// fresh session seeded from that doc.
///
/// On any failure before the doc is ready the pane is left untouched — we never
/// retire a session unless its handoff file is present and stable.
pub fn perform_handoff<S>(
    tmux: &dyn TmuxControl,
    opts: &HandoffOptions,
    mut sleep_fn: S,
) -> anyhow::Result<HandoffOutcome>
where
    S: FnMut(Duration),
{
    // Never start while the user has unsent text in the box — see
    // user_input_pending. Reported as Superseded so the caller backs off
    // politely instead of counting a failure.
    if user_input_pending(tmux, &opts.pane) {
        return Ok(HandoffOutcome::Superseded);
    }

    std::fs::create_dir_all(&opts.handoff_dir)?;
    let handoff_path = expected_handoff_path(&opts.handoff_dir, &opts.session_id);
    // Stale file from a prior aborted attempt must not be mistaken for success.
    let _ = std::fs::remove_file(&handoff_path);

    // Capture the original session's launch flags while it is still alive, so the
    // successor inherits them (best-effort: no flags on failure).
    let launch_flags = sanitize_launch_flags(&tmux.pane_launch_flags(&opts.pane).unwrap_or_default());

    // The doc path appears in our prompt and nowhere else, so it identifies our
    // own injected prompt among the session's other prompts.
    let marker = handoff_path.to_string_lossy().to_string();
    let prompt = handoff_prompt(&handoff_path);
    let baseline_prompts = prompt_stats(&opts.transcript_path, &marker)?.prompts;

    send_prompt_verified(
        tmux,
        &opts.pane,
        &opts.transcript_path,
        &prompt,
        &marker,
        &mut sleep_fn,
    )?;

    let ready = await_handoff_doc(
        &handoff_path,
        &opts.transcript_path,
        &marker,
        baseline_prompts,
        opts.timeout_secs,
        &mut sleep_fn,
    )?;
    if !ready {
        return Ok(HandoffOutcome::Superseded);
    }

    launch_successor(tmux, &opts.pane, &seed_command(&handoff_path, &launch_flags), &mut sleep_fn)?;
    Ok(HandoffOutcome::Completed(handoff_path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::FakeTmux;

    const PANE: &str = "%5";

    fn new_transcript(dir: &Path) -> PathBuf {
        let transcript = dir.join("sess.jsonl");
        std::fs::write(&transcript, "{\"type\":\"assistant\",\"message\":{}}\n").unwrap();
        transcript
    }

    fn append_prompt(transcript: &Path, text: &str) {
        let line = serde_json::json!({"type":"user","message":{"content":text}});
        let mut s = std::fs::read_to_string(transcript).unwrap_or_default();
        s.push_str(&format!("{line}\n"));
        std::fs::write(transcript, s).unwrap();
    }

    /// What the live session does, driven off the injected clock: once our prompt
    /// has actually been submitted, record it in the transcript (as Claude Code
    /// would), then take `then` — write the doc, or let the human barge in.
    enum ThenDo {
        WriteDoc(PathBuf),
        UserBargesIn,
        Nothing,
    }

    fn session_driver<'a>(
        fake: &'a FakeTmux,
        transcript: PathBuf,
        prompt: String,
        then: ThenDo,
    ) -> impl FnMut(Duration) + 'a {
        let mut recorded = false;
        let mut acted = false;
        move |_d: Duration| {
            if !recorded {
                // FakeTmux moves the input box into the scrollback on Enter, so
                // the prompt showing up there means it was really submitted.
                let submitted = fake
                    .capture_pane(PANE)
                    .map(|c| contains_ignoring_whitespace(&c, &prompt))
                    .unwrap_or(false)
                    && fake.calls().iter().any(|c| c == "send_enter:%5");
                if submitted {
                    append_prompt(&transcript, &prompt);
                    recorded = true;
                }
                return;
            }
            match &then {
                ThenDo::WriteDoc(p) => {
                    let _ = std::fs::write(p, "handoff body");
                }
                ThenDo::UserBargesIn => {
                    if !acted {
                        acted = true;
                        append_prompt(&transcript, "actually hold off — I'm still working here");
                    }
                }
                ThenDo::Nothing => {}
            }
        }
    }

    fn opts(dir: &Path, transcript: &Path, timeout: u64) -> HandoffOptions {
        HandoffOptions {
            pane: PANE.into(),
            session_id: "sess-9".into(),
            handoff_dir: dir.join("handoffs"),
            timeout_secs: timeout,
            transcript_path: transcript.to_path_buf(),
        }
    }

    #[test]
    fn drives_session_then_respawns_seeded() {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeTmux::new();
        let transcript = new_transcript(dir.path());
        // The original session's raw argv: real flags + a value flag + a
        // session-continuation flag + a stale positional prompt. The successor
        // must inherit the real flags but NOT --continue or the stale prompt.
        fake.set_launch_flags(vec![
            "--dangerously-skip-permissions".into(),
            "--continue".into(),
            "--model".into(), "opus".into(),
            "Reply with READY then wait.".into(),
        ]);

        let o = opts(dir.path(), &transcript, 20);
        let expected = expected_handoff_path(&o.handoff_dir, "sess-9");
        let driver = session_driver(
            &fake,
            transcript.clone(),
            handoff_prompt(&expected),
            ThenDo::WriteDoc(expected.clone()),
        );

        match perform_handoff(&fake, &o, driver).unwrap() {
            HandoffOutcome::Completed(p) => assert_eq!(p, expected),
            other => panic!("expected Completed, got {other:?}"),
        }

        let calls = fake.calls();
        // The prompt is typed, verified, then submitted.
        assert!(calls.iter().any(|c| c.starts_with("send_text:%5:Write a complete handoff")));
        assert!(calls.iter().any(|c| c == "send_enter:%5"));
        // Then the pane is respawned into a shell...
        assert!(calls.iter().any(|c| c == "respawn_shell:%5"));
        // ...and the seeded claude command carries the original launch flags.
        let seed = calls
            .iter()
            .rfind(|c| c.starts_with("send_text:%5:claude"))
            .expect("expected a seeded claude command");
        assert!(seed.contains("--dangerously-skip-permissions"));
        assert!(seed.contains("--model opus"));
        assert!(seed.contains(expected.to_str().unwrap()));
        // Stale positional prompt and --continue must NOT leak into the successor.
        assert!(!seed.contains("Reply with READY"));
        assert!(!seed.contains("--continue"));
    }

    #[test]
    fn retries_when_first_keystrokes_are_swallowed() {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeTmux::new();
        let transcript = new_transcript(dir.path());
        // The TUI is cold: it drops the first send entirely.
        fake.swallow_next_sends(1);

        let o = opts(dir.path(), &transcript, 20);
        let expected = expected_handoff_path(&o.handoff_dir, "sess-9");
        let driver = session_driver(
            &fake,
            transcript.clone(),
            handoff_prompt(&expected),
            ThenDo::WriteDoc(expected.clone()),
        );

        let result = perform_handoff(&fake, &o, driver).unwrap();
        assert!(matches!(result, HandoffOutcome::Completed(_)), "got {result:?}");

        // Two prompt sends: the swallowed one and the successful retry.
        let prompt_sends = fake
            .calls()
            .iter()
            .filter(|c| c.starts_with("send_text:%5:Write a complete handoff"))
            .count();
        assert_eq!(prompt_sends, 2, "expected exactly one retry after the swallowed send");
    }

    #[test]
    fn stands_down_when_user_takes_over() {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeTmux::new();
        let transcript = new_transcript(dir.path());

        let o = opts(dir.path(), &transcript, 30);
        let expected = expected_handoff_path(&o.handoff_dir, "sess-9");
        let driver = session_driver(
            &fake,
            transcript.clone(),
            handoff_prompt(&expected),
            ThenDo::UserBargesIn,
        );

        let result = perform_handoff(&fake, &o, driver).unwrap();
        assert!(matches!(result, HandoffOutcome::Superseded), "got {result:?}");
        // Standing down must never retire the session.
        assert!(
            !fake.calls().iter().any(|c| c.starts_with("respawn_shell:")),
            "must not retire a session the user has taken back"
        );
    }

    #[test]
    fn fails_loudly_when_successor_never_starts() {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeTmux::new();
        let transcript = new_transcript(dir.path());
        // Typing the claude command into the shell does not bring claude up.
        fake.set_successor_starts(false);

        let o = opts(dir.path(), &transcript, 20);
        let expected = expected_handoff_path(&o.handoff_dir, "sess-9");
        let driver = session_driver(
            &fake,
            transcript.clone(),
            handoff_prompt(&expected),
            ThenDo::WriteDoc(expected.clone()),
        );

        let err = perform_handoff(&fake, &o, driver).unwrap_err();
        assert!(
            err.to_string().contains("successor claude never started"),
            "unexpected error: {err}"
        );
        // The user must be told, since their old session is already gone.
        assert!(
            fake.calls()
                .iter()
                .any(|c| c.starts_with("display_message:") && c.contains("successor failed")),
            "expected a recovery message on the pane"
        );
    }

    /// Shape copied from real `tmux capture-pane` output of a Claude Code pane.
    const REAL_CAPTURE: &str = "\
❯ Reply with exactly OK1

● OK1

✻ Baked for 3s

────────────────────────────────────────
❯ Write a complete handoff document to /h/sess-9.md using the Write tool. Capture the current task, what
  has been done, and the exact next steps.
────────────────────────────────────────
  [OMC#4.15.4] | Model: Opus 5 | ctx:[#---------]5%
  ⏵⏵ bypass permissions on (shift+tab to cycle)
";

    #[test]
    fn input_box_text_reads_only_the_framed_area() {
        let box_text = input_box_text(REAL_CAPTURE).expect("expected a framed input area");
        assert!(box_text.starts_with("Write a complete handoff document"));
        // The scrollback above the frame must not leak in.
        assert!(!box_text.contains("Reply with exactly OK1"));
        assert!(!box_text.contains("bypass permissions"));
        // Wrapped continuation rows are joined back together.
        assert!(box_text.contains("exact next steps"));
    }

    #[test]
    fn input_box_text_is_empty_for_an_empty_box_and_none_without_a_frame() {
        let empty = "some output\n────────────────────────\n❯ \n────────────────────────\n";
        assert_eq!(input_box_text(empty).as_deref(), Some(""));
        // A plain shell prompt has no frame at all.
        assert_eq!(input_box_text("user@host ~ % claude --model opus\n"), None);
    }

    #[test]
    fn does_not_start_when_user_has_unsent_text() {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeTmux::new();
        let transcript = new_transcript(dir.path());
        // The user has typed something that has not been submitted yet — as
        // happens when they type while the model is mid-turn and the TUI queues it.
        fake.send_text(PANE, "wait, I'm still using this").unwrap();

        let o = opts(dir.path(), &transcript, 20);
        let sleep_fn = |_d: Duration| {};
        let result = perform_handoff(&fake, &o, sleep_fn).unwrap();
        assert!(matches!(result, HandoffOutcome::Superseded), "got {result:?}");
        // Their text must survive: no clear_input, no prompt, no respawn.
        let calls = fake.calls();
        assert!(!calls.iter().any(|c| c.starts_with("clear_input:")), "must not clear the user's text");
        assert!(!calls.iter().any(|c| c.contains("Write a complete handoff")));
        assert!(!calls.iter().any(|c| c.starts_with("respawn_shell:")));
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
        let fake = FakeTmux::new();
        let transcript = new_transcript(dir.path());
        let o = opts(dir.path(), &transcript, 3);
        let expected = expected_handoff_path(&o.handoff_dir, "sess-9");
        // Prompt is delivered and submitted, but the doc is never written.
        let driver = session_driver(&fake, transcript.clone(), handoff_prompt(&expected), ThenDo::Nothing);

        let err = perform_handoff(&fake, &o, driver).unwrap_err();
        assert!(err.to_string().contains("did not stabilize"), "got: {err}");
        // It must NOT have relaunched the pane on failure.
        assert!(!fake.calls().iter().any(|c| c.starts_with("respawn_shell:")));
    }

    #[test]
    fn errors_when_prompt_can_never_be_delivered() {
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeTmux::new();
        let transcript = new_transcript(dir.path());
        // Every send is dropped, so the prompt never lands.
        fake.swallow_next_sends(u32::MAX);

        let o = opts(dir.path(), &transcript, 5);
        let sleep_fn = |_d: Duration| {};
        let err = perform_handoff(&fake, &o, sleep_fn).unwrap_err();
        assert!(err.to_string().contains("could not deliver handoff prompt"), "got: {err}");
        // Never retire a session whose prompt we could not even deliver.
        assert!(!fake.calls().iter().any(|c| c.starts_with("respawn_shell:")));
    }
}
