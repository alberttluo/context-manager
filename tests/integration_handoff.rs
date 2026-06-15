use context_manager::config::Config;
use context_manager::daemon::{Daemon, MtimeTracker};
use context_manager::monitor::SessionMonitor;
use context_manager::paths::Paths;
use context_manager::registration::{self, Registration};
use context_manager::tmux::FakeTmux;
use std::collections::HashMap;
use std::time::Instant;

#[test]
fn dry_run_decides_handoff_for_over_threshold_quiet_session() {
    let base = tempfile::tempdir().unwrap();
    let config_dir = base.path().join("config");
    let state_dir = base.path().join("state");
    let paths = Paths::with_base(config_dir, state_dir);

    // A transcript already over threshold (120k of a 200k window = 60%), last
    // entry is an assistant turn.
    let transcript = base.path().join("sess.jsonl");
    std::fs::write(&transcript,
        "{\"type\":\"assistant\",\"message\":{\"model\":\"m\",\"usage\":{\"cache_read_input_tokens\":120000}}}\n").unwrap();

    let reg = Registration {
        session_id: "sess".into(),
        transcript_path: transcript,
        cwd: "/tmp".into(),
        tmux_pane: "%1".into(),
        pid: 1,
        started_at: "2026-06-15T12:00:00Z".into(),
    };
    registration::write(&paths.sessions_dir(), &reg).unwrap();

    // quiet_period_secs = 0 and grace_secs = 0 so the state machine can reach
    // ExecuteHandoff within two ticks of the same logical instant.
    let config = Config { dry_run: true, quiet_period_secs: 0, grace_secs: 0, ..Default::default() };

    let fake = FakeTmux::new();
    let daemon = Daemon { config, paths: &paths, tmux: &fake, claude_projects_dir: base.path().join("claude-projects") };

    let mut monitors: HashMap<String, SessionMonitor> = HashMap::new();
    let mut mtimes = MtimeTracker::default();

    let t0 = Instant::now();
    // Tick 1: first observation registers baseline mtime (counts as change) -> Idle.
    daemon.tick(t0, &mut monitors, &mut mtimes).unwrap();
    // Tick 2: no change, quiet>=0, over threshold -> NotifyGrace.
    daemon.tick(t0, &mut monitors, &mut mtimes).unwrap();
    // Tick 3: grace>=0 elapsed -> ExecuteHandoff (dry-run logs lineage).
    daemon.tick(t0, &mut monitors, &mut mtimes).unwrap();

    // Dry-run never touches tmux.
    assert!(fake.calls().is_empty());
    // Lineage recorded a dry-run handoff.
    let lineage = std::fs::read_to_string(paths.lineage_file()).unwrap();
    assert!(lineage.contains("\"dry_run\":true"));
    assert!(lineage.contains("\"from_session\":\"sess\""));
}
