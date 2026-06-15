use anyhow::Result;
use clap::Parser;
use context_manager::config::Config;
use context_manager::daemon::Daemon;
use context_manager::paths::Paths;
use context_manager::tmux::RealTmux;
use std::collections::HashMap;
use std::time::Instant;

#[derive(Parser)]
#[command(name = "context-managerd", about = "Background manager for Claude Code sessions")]
struct Args {
    /// Override the config file path.
    #[arg(long)]
    config: Option<std::path::PathBuf>,
    /// Log decisions without performing any handoff.
    #[arg(long)]
    dry_run: bool,
    /// Run a single tick and exit (for testing).
    #[arg(long)]
    once: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let paths = Paths::resolve()?;

    let config_path = args.config.unwrap_or_else(|| paths.config_file());
    let mut config = Config::load(&config_path)?;
    if args.dry_run {
        config.dry_run = true;
    }

    eprintln!(
        "[cm] starting; config={} threshold={:.0}% dry_run={} poll={}s",
        config_path.display(), config.threshold * 100.0, config.dry_run, config.poll_interval_secs
    );

    let tmux = RealTmux;
    let claude_projects_dir = directories::BaseDirs::new()
        .map(|b| b.home_dir().join(".claude/projects"))
        .unwrap_or_default();
    let daemon = Daemon { config, paths: &paths, tmux: &tmux, claude_projects_dir };

    if args.once {
        let mut monitors = HashMap::new();
        let mut mtimes = Default::default();
        daemon.tick(Instant::now(), &mut monitors, &mut mtimes)?;
        return Ok(());
    }

    daemon.run()
}
