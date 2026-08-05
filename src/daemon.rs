use crate::config::Config;
use crate::handoff::{perform_handoff, HandoffOptions, HandoffOutcome};
use crate::lineage::{self, LineageRecord};
use crate::model_window::resolve_window;
use crate::monitor::{SessionMonitor, TickInput, TickOutcome, MAX_HANDOFF_FAILURES};
use crate::paths::Paths;
use crate::registration::{self, Registration};
use crate::transcript;
use crate::tmux::TmuxControl;
use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

/// Tracks the last-seen mtime per session to derive `transcript_changed`.
#[derive(Default)]
pub struct MtimeTracker {
    last: HashMap<String, u64>,
}

impl MtimeTracker {
    pub fn changed(&mut self, session_id: &str, mtime: Option<u64>) -> bool {
        let Some(m) = mtime else { return false };
        match self.last.insert(session_id.to_string(), m) {
            Some(prev) => prev != m,
            None => true, // first observation counts as a change (resets quiet clock)
        }
    }
}

fn mtime_secs(path: &Path) -> Option<u64> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    let dur = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(dur.as_secs())
}

pub struct Daemon<'a> {
    pub config: Config,
    pub paths: &'a Paths,
    pub tmux: &'a dyn TmuxControl,
    pub claude_projects_dir: std::path::PathBuf,
}

impl<'a> Daemon<'a> {
    /// Run one scan+evaluate pass over all registered sessions.
    pub fn tick(
        &self,
        now: Instant,
        monitors: &mut HashMap<String, SessionMonitor>,
        mtimes: &mut MtimeTracker,
    ) -> anyhow::Result<()> {
        let regs = registration::scan(&self.paths.sessions_dir())?;
        let live_ids: std::collections::HashSet<String> =
            regs.iter().map(|r| r.session_id.clone()).collect();

        // Drop monitors for sessions that have vanished.
        monitors.retain(|id, _| live_ids.contains(id));

        for reg in &regs {
            if let Err(e) = self.evaluate_session(now, reg, monitors, mtimes) {
                eprintln!("[cm] session {} error: {e:#}", reg.session_id);
            }
        }
        Ok(())
    }

    fn evaluate_session(
        &self,
        now: Instant,
        reg: &Registration,
        monitors: &mut HashMap<String, SessionMonitor>,
        mtimes: &mut MtimeTracker,
    ) -> anyhow::Result<()> {
        // Enforce the ignore list here rather than only in discovery: sessions
        // also arrive via the SessionStart hook, which has no config, so an
        // ignored cwd was still being handed off through that path.
        if self.is_ignored(&reg.cwd.to_string_lossy()) {
            return Ok(());
        }
        let state = transcript::analyze(&reg.transcript_path)?;
        let window = resolve_window(state.model.as_deref(), state.max_context_tokens, &self.config);
        let pct = if window.tokens == 0 { 0.0 } else { state.context_tokens as f64 / window.tokens as f64 };
        let changed = mtimes.changed(&reg.session_id, mtime_secs(&reg.transcript_path));

        let monitor = monitors.entry(reg.session_id.clone()).or_insert_with(|| SessionMonitor::new(now));

        // Say out loud when the window is a guess. It is the one input here that
        // can be badly wrong while every other sign of health looks fine, and a
        // model id gives no hint of its own window: Claude Code writes the 1M
        // Opus variant as plain "claude-opus-5", exactly what a 200k session
        // reports. Guessing 200k for a 1M session hands it off at 9% of its real
        // context, and the empirical correction never rescues it — the session is
        // retired long before it can be observed above 200k.
        //
        // Warn rather than refuse: for a model whose window really is the
        // default this estimate is correct, and refusing would leave those
        // sessions unmanaged forever, since observation can never confirm a
        // window it does not exceed.
        if window.is_guess() {
            if let Some(model) = state.model.as_deref() {
                if monitor.note_guessed_window(model) {
                    eprintln!(
                        "[cm] WARNING: unknown model {model:?} — assuming the {}-token default \
                         and handing off at {:.0}%. Add it to [model_windows] if that is wrong.",
                        window.tokens,
                        self.config.threshold * 100.0,
                    );
                }
            }
        }
        let outcome = monitor.tick(now, &TickInput {
            context_pct: pct,
            threshold: self.config.threshold,
            last_entry: state.last_entry,
            transcript_changed: changed,
            quiet_period_secs: self.config.quiet_period_secs,
            grace_secs: self.config.grace_secs,
            cooldown_secs: self.config.cooldown_secs,
        });

        match outcome {
            TickOutcome::Idle | TickOutcome::CancelGrace => {}
            TickOutcome::NotifyGrace => {
                self.notify_grace(reg);
            }
            TickOutcome::ExecuteHandoff => {
                self.execute(now, reg, pct, monitor)?;
            }
        }
        Ok(())
    }

    /// Whether a cwd is excluded from management. Matching mirrors discovery's
    /// substring test so both registration paths agree.
    fn is_ignored(&self, cwd: &str) -> bool {
        self.config.ignore_cwds.iter().any(|ig| cwd.contains(ig.as_str()))
    }

    fn notify_grace(&self, reg: &Registration) {
        let msg = format!(
            "[context-manager] context high; handing off in {}s — type to defer",
            self.config.grace_secs
        );
        // Best-effort, non-fatal: display a tmux message on the pane.
        // Skipped in dry-run so tests and observation-only mode never touch tmux.
        if !self.config.dry_run {
            let _ = self.tmux.display_message(&reg.tmux_pane, &msg);
        }
        eprintln!("{msg} (session {})", reg.session_id);
    }

    fn execute(
        &self,
        now: Instant,
        reg: &Registration,
        pct: f64,
        monitor: &mut SessionMonitor,
    ) -> anyhow::Result<()> {
        if self.config.dry_run {
            eprintln!("[cm] DRY-RUN would hand off session {} (pane {}, {:.0}%)",
                reg.session_id, reg.tmux_pane, pct * 100.0);
            monitor.note_handoff_done(now);
            self.log_lineage(reg, pct, "dry-run", true);
            return Ok(());
        }

        let opts = HandoffOptions {
            pane: reg.tmux_pane.clone(),
            session_id: reg.session_id.clone(),
            handoff_dir: self.paths.handoff_dir(),
            timeout_secs: self.config.handoff_timeout_secs,
            transcript_path: reg.transcript_path.clone(),
        };
        match perform_handoff(self.tmux, &opts, |d: Duration| std::thread::sleep(d)) {
            Ok(HandoffOutcome::Completed(handoff_path)) => {
                monitor.note_handoff_done(now);
                // The old session is being retired; remove its registration so we
                // stop evaluating it. The successor re-registers via its own hook.
                let _ = registration::remove(&self.paths.sessions_dir(), &reg.session_id);
                self.log_lineage(reg, pct, &handoff_path.to_string_lossy(), false);
                eprintln!("[cm] handed off session {} -> pane {}", reg.session_id, reg.tmux_pane);
            }
            Ok(HandoffOutcome::Superseded) => {
                // The human is driving again. Back off silently; the next quiet
                // period will reconsider. Not a failure, so no backoff escalation.
                monitor.note_superseded(now);
                eprintln!(
                    "[cm] handoff superseded by user activity for session {} (session left intact)",
                    reg.session_id
                );
            }
            Err(e) => {
                // Leave the session untouched and start (escalating) cooldown.
                let failures = monitor.note_handoff_failed(now);
                eprintln!(
                    "[cm] handoff FAILED for session {} (attempt {failures}/{MAX_HANDOFF_FAILURES}): {e:#} (session left intact)",
                    reg.session_id
                );
                // Failures were previously invisible in the lineage log, which
                // made repeated attempts impossible to spot after the fact.
                self.log_lineage(reg, pct, &format!("FAILED: {e}"), false);
                if monitor.is_abandoned() {
                    eprintln!(
                        "[cm] giving up on session {} after {MAX_HANDOFF_FAILURES} failed handoffs",
                        reg.session_id
                    );
                    let _ = self.tmux.display_message(
                        &reg.tmux_pane,
                        "[context-manager] handoff failed repeatedly — managing this session is paused",
                    );
                }
            }
        }
        Ok(())
    }

    fn log_lineage(&self, reg: &Registration, pct: f64, handoff_path: &str, dry_run: bool) {
        let rec = LineageRecord {
            ts: chrono::Utc::now().to_rfc3339(),
            from_session: reg.session_id.clone(),
            to_pane: reg.tmux_pane.clone(),
            handoff_path: handoff_path.to_string(),
            context_pct: pct,
            dry_run,
        };
        if let Err(e) = lineage::append(&self.paths.lineage_file(), &rec) {
            eprintln!("[cm] failed to write lineage: {e:#}");
        }
    }

    pub fn discover_and_register(&self) -> anyhow::Result<usize> {
        let panes = self.tmux.list_panes()?;
        let live_pane_ids: std::collections::HashSet<&str> =
            panes.iter().map(|p| p.pane_id.as_str()).collect();
        let found = crate::discovery::discover_sessions(
            &panes,
            &self.claude_projects_dir,
            &self.config.ignore_cwds,
        );
        // Authoritative pane -> current session from this scan.
        let current_by_pane: HashMap<&str, &str> = found
            .iter()
            .map(|r| (r.tmux_pane.as_str(), r.session_id.as_str()))
            .collect();
        let dir = self.paths.sessions_dir();

        // Reconcile: a pane has exactly one session. Drop any registration whose
        // pane now runs a different session (e.g. the old one after a handoff or
        // a /clear), or whose pane no longer exists. This dedups by pane and
        // prunes dead panes — registration files otherwise only vanish on the
        // SessionEnd hook or a successful handoff.
        for reg in crate::registration::scan(&dir).unwrap_or_default() {
            let stale = match current_by_pane.get(reg.tmux_pane.as_str()) {
                Some(&current) => current != reg.session_id,
                None => !live_pane_ids.contains(reg.tmux_pane.as_str()),
            };
            if stale {
                let _ = crate::registration::remove(&dir, &reg.session_id);
            }
        }

        let mut n = 0;
        for reg in found {
            let path = dir.join(format!("{}.json", reg.session_id));
            if path.exists() {
                continue;
            }
            if crate::registration::write(&dir, &reg).is_ok() {
                n += 1;
            }
        }
        Ok(n)
    }

    /// Block forever, ticking every `poll_interval_secs`.
    pub fn run(&self) -> anyhow::Result<()> {
        let mut monitors: HashMap<String, SessionMonitor> = HashMap::new();
        let mut mtimes = MtimeTracker::default();
        let interval = Duration::from_secs(self.config.poll_interval_secs.max(1));
        let discovery_interval = Duration::from_secs(self.config.discovery_interval_secs.max(1));
        let mut last_discovery = Instant::now() - discovery_interval; // trigger immediately
        loop {
            if last_discovery.elapsed() >= discovery_interval {
                match self.discover_and_register() {
                    Ok(n) if n > 0 => eprintln!("[cm] discovered {n} new session(s)"),
                    Ok(_) => {}
                    Err(e) => eprintln!("[cm] discovery error: {e:#}"),
                }
                last_discovery = Instant::now();
            }
            if let Err(e) = self.tick(Instant::now(), &mut monitors, &mut mtimes) {
                eprintln!("[cm] tick error: {e:#}");
            }
            std::thread::sleep(interval);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registration::{self as registration, Registration};
    use crate::tmux::{FakeTmux, PaneInfo};
    use crate::monitor::SessionMonitor;

    #[test]
    fn detects_mtime_change() {
        let mut tracker = MtimeTracker::default();
        // First observation of a session: treated as a change (establishes baseline).
        assert!(tracker.changed("sess", Some(100)));
        // Same mtime: no change.
        assert!(!tracker.changed("sess", Some(100)));
        // New mtime: change.
        assert!(tracker.changed("sess", Some(200)));
        // Missing mtime (file gone): no change reported.
        assert!(!tracker.changed("sess", None));
    }

    #[test]
    fn notify_grace_displays_message_on_real_session() {
        let base = tempfile::tempdir().unwrap();
        let config_dir = base.path().join("config");
        let state_dir = base.path().join("state");
        let paths = crate::paths::Paths::with_base(config_dir, state_dir);

        // Transcript over threshold (120k of ~200k window = 60%), last entry assistant.
        let transcript = base.path().join("sess.jsonl");
        std::fs::write(
            &transcript,
            "{\"type\":\"assistant\",\"message\":{\"model\":\"m\",\"usage\":{\"cache_read_input_tokens\":120000}}}\n",
        )
        .unwrap();

        let reg = Registration {
            session_id: "sess".into(),
            transcript_path: transcript,
            cwd: "/tmp".into(),
            tmux_pane: "%1".into(),
            pid: 1,
            started_at: "2026-06-15T12:00:00Z".into(),
        };
        registration::write(&paths.sessions_dir(), &reg).unwrap();

        // dry_run = false so notify_grace actually calls display_message.
        // grace_secs = 100 keeps the state machine at NotifyGrace (never reaches
        // ExecuteHandoff at the same logical Instant), so respawn is never called.
        let config = Config {
            dry_run: false,
            quiet_period_secs: 0,
            grace_secs: 100,
            ..Default::default()
        };

        let fake = FakeTmux::new();
        let projects_dir = tempfile::tempdir().unwrap();
        let daemon = Daemon {
            config,
            paths: &paths,
            tmux: &fake,
            claude_projects_dir: projects_dir.path().to_path_buf(),
        };

        let mut monitors: HashMap<String, SessionMonitor> = HashMap::new();
        let mut mtimes = MtimeTracker::default();

        let t0 = Instant::now();
        // Tick 1: first observation registers baseline mtime (counts as change) -> Idle.
        daemon.tick(t0, &mut monitors, &mut mtimes).unwrap();
        // Tick 2: no change, quiet>=0, over threshold -> NotifyGrace.
        daemon.tick(t0, &mut monitors, &mut mtimes).unwrap();

        let calls = fake.calls();
        let display_call = calls.iter().find(|c| c.starts_with("display_message:"));
        assert!(
            display_call.is_some(),
            "expected a display_message call, got: {calls:?}"
        );
        assert!(
            display_call.unwrap().contains("defer"),
            "expected message to mention deferring, got: {display_call:?}"
        );
        // Grace window not elapsed — no respawn should have been issued.
        assert!(
            !calls.iter().any(|c| c.starts_with("respawn:")),
            "unexpected respawn call: {calls:?}"
        );
    }

    #[test]
    fn ignored_cwd_is_never_handed_off_even_when_hook_registered() {
        let base = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::with_base(
            base.path().join("config"),
            base.path().join("state"),
        );

        // Over threshold, idle, sitting at a finished assistant turn — eligible
        // in every respect except that its cwd is excluded.
        let transcript = base.path().join("sess.jsonl");
        std::fs::write(
            &transcript,
            "{\"type\":\"assistant\",\"message\":{\"model\":\"m\",\"usage\":{\"cache_read_input_tokens\":180000}}}\n",
        )
        .unwrap();

        // The SessionStart hook has no config, so it registers ignored cwds too.
        registration::write(&paths.sessions_dir(), &Registration {
            session_id: "ignored-sess".into(),
            transcript_path: transcript,
            cwd: "/mnt/c/Users/someone/Desktop/excluded-project".into(),
            tmux_pane: "%9".into(),
            pid: 1,
            started_at: "2026-07-25T12:00:00Z".into(),
        }).unwrap();

        let config = Config {
            dry_run: false,
            quiet_period_secs: 0,
            grace_secs: 0,
            ignore_cwds: vec!["/mnt/c/Users/someone/Desktop/excluded-project".to_string()],
            ..Default::default()
        };

        let fake = FakeTmux::new();
        let projects_dir = tempfile::tempdir().unwrap();
        let daemon = Daemon {
            config,
            paths: &paths,
            tmux: &fake,
            claude_projects_dir: projects_dir.path().to_path_buf(),
        };

        let mut monitors: HashMap<String, SessionMonitor> = HashMap::new();
        let mut mtimes = MtimeTracker::default();
        let t0 = Instant::now();
        // Several ticks: with grace_secs = 0 an unfiltered session would reach
        // ExecuteHandoff almost immediately.
        for _ in 0..4 {
            daemon.tick(t0, &mut monitors, &mut mtimes).unwrap();
        }

        assert!(
            fake.calls().is_empty(),
            "ignored cwd must never be touched, got: {:?}",
            fake.calls()
        );
    }

    #[test]
    fn discovery_reconciles_pane_to_single_session() {
        let base = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::with_base(
            base.path().join("config"),
            base.path().join("state"),
        );
        let projects = tempfile::tempdir().unwrap();

        // Pane %7 (cwd /work/proj) currently runs session "sessNew".
        let proj_dir = projects.path().join(crate::discovery::encode_project_dir("/work/proj"));
        std::fs::create_dir_all(&proj_dir).unwrap();
        std::fs::write(proj_dir.join("sessNew.jsonl"), "{}\n").unwrap();

        let sdir = paths.sessions_dir();
        // Stale leftover for the SAME pane but an older session.
        registration::write(&sdir, &Registration {
            session_id: "sessOld".into(),
            transcript_path: "/old.jsonl".into(),
            cwd: "/work/proj".into(),
            tmux_pane: "%7".into(),
            pid: 0,
            started_at: "x".into(),
        }).unwrap();
        // Registration for a pane that no longer exists.
        registration::write(&sdir, &Registration {
            session_id: "ghost".into(),
            transcript_path: "/ghost.jsonl".into(),
            cwd: "/gone".into(),
            tmux_pane: "%99".into(),
            pid: 0,
            started_at: "x".into(),
        }).unwrap();

        let fake = FakeTmux::new();
        fake.set_panes(vec![PaneInfo {
            pane_id: "%7".into(),
            cwd: "/work/proj".into(),
            command: "claude".into(),
        }]);
        let daemon = Daemon {
            config: Config::default(),
            paths: &paths,
            tmux: &fake,
            claude_projects_dir: projects.path().to_path_buf(),
        };

        daemon.discover_and_register().unwrap();

        let regs = registration::scan(&sdir).unwrap();
        let ids: Vec<&str> = regs.iter().map(|r| r.session_id.as_str()).collect();
        // Stale same-pane session and dead-pane session pruned; only current remains.
        assert_eq!(ids, vec!["sessNew"], "expected only the current session, got {ids:?}");
        assert_eq!(regs[0].tmux_pane, "%7");
    }
}
