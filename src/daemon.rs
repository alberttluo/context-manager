use crate::config::Config;
use crate::handoff::{perform_handoff, HandoffOptions};
use crate::lineage::{self, LineageRecord};
use crate::model_window::resolve_window;
use crate::monitor::{SessionMonitor, TickInput, TickOutcome};
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
        let state = transcript::analyze(&reg.transcript_path)?;
        let window = resolve_window(state.model.as_deref(), &self.config);
        let pct = if window == 0 { 0.0 } else { state.context_tokens as f64 / window as f64 };
        let changed = mtimes.changed(&reg.session_id, mtime_secs(&reg.transcript_path));

        let monitor = monitors.entry(reg.session_id.clone()).or_insert_with(|| SessionMonitor::new(now));
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
        };
        match perform_handoff(self.tmux, &opts, |d: Duration| std::thread::sleep(d)) {
            Ok(handoff_path) => {
                monitor.note_handoff_done(now);
                // The old session is being retired; remove its registration so we
                // stop evaluating it. The successor re-registers via its own hook.
                let _ = registration::remove(&self.paths.sessions_dir(), &reg.session_id);
                self.log_lineage(reg, pct, &handoff_path.to_string_lossy(), false);
                eprintln!("[cm] handed off session {} -> pane {}", reg.session_id, reg.tmux_pane);
            }
            Err(e) => {
                // Abort cleanly: leave the session untouched, start cooldown to
                // avoid retry storms, log the failure.
                monitor.note_handoff_done(now);
                eprintln!("[cm] handoff FAILED for session {}: {e:#} (session left intact)", reg.session_id);
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

    /// Block forever, ticking every `poll_interval_secs`.
    pub fn run(&self) -> anyhow::Result<()> {
        let mut monitors: HashMap<String, SessionMonitor> = HashMap::new();
        let mut mtimes = MtimeTracker::default();
        let interval = Duration::from_secs(self.config.poll_interval_secs.max(1));
        loop {
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
    use crate::tmux::FakeTmux;
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
        let daemon = Daemon { config, paths: &paths, tmux: &fake };

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
}
