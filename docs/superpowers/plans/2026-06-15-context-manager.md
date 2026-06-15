# Context Manager Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a Rust `systemctl --user` daemon that watches every interactive Claude Code session and, when one crosses a context threshold at a safe idle boundary, automatically hands it off and respawns a fresh session in the same tmux pane.

**Architecture:** A registration hook (`cm-hook`) runs on Claude's `SessionStart`/`SessionEnd` and drops a JSON record (`session_id`, `transcript_path`, `cwd`, `tmux_pane`, `pid`) into a state dir. The daemon (`context-managerd`) polls those records and their JSONL transcripts every few seconds, computes context %, and runs a per-session state machine (threshold → quiet period → grace window → handoff). Handoff is performed by driving the live session via tmux (`send-keys` to write a handoff doc, then `respawn-pane -k` to launch a fresh seeded session). All tmux access goes through a `TmuxControl` trait so orchestration is unit-testable.

**Tech Stack:** Rust 2021, `serde`/`serde_json` (JSONL), `toml` (config), `chrono` (timestamps), `anyhow` (errors), `clap` (CLI), `directories` (XDG paths). tmux as the terminal multiplexer. systemd `--user` for service management.

---

## File Structure

A single binary crate exposing a library plus two binaries.

```
Cargo.toml
src/
  lib.rs              # re-exports all modules
  config.rs           # Config struct, defaults, TOML load
  usage.rs            # Usage struct, parse one transcript line, effective context tokens
  model_window.rs     # resolve model id -> context window
  transcript.rs       # analyze a transcript file -> TranscriptState
  registration.rs     # Registration struct, scan/write/remove in the sessions dir
  decision.rs         # pure eligibility logic (threshold + quiet + cooldown)
  monitor.rs          # SessionMonitor state machine (grace/cooldown), clock injected
  tmux.rs             # TmuxControl trait, RealTmux (shells out), FakeTmux (tests)
  lineage.rs          # LineageRecord + append to lineage.jsonl
  handoff.rs          # perform_handoff: drive live session + respawn via TmuxControl
  paths.rs            # XDG path helpers (config/state/handoff dirs)
  daemon.rs           # the poll loop wiring everything together
  bin/
    context-managerd.rs   # daemon entrypoint (clap args, load config, run loop)
    cm-hook.rs            # hook entrypoint (stdin JSON + env -> write/remove registration)
tests/
  fixtures/             # sample JSONL transcripts
  integration_handoff.rs
deploy/
  context-manager.service
  install-hooks.md
```

Each module has one responsibility and is testable in isolation. `daemon.rs` is the only piece needing live integration validation; everything it calls is unit-tested.

---

## Task 1: Project scaffold

**Files:**
- Create: `Cargo.toml`
- Create: `src/lib.rs`
- Create: `src/bin/context-managerd.rs`
- Create: `src/bin/cm-hook.rs`

- [ ] **Step 1: Create `Cargo.toml`**

```toml
[package]
name = "context-manager"
version = "0.1.0"
edition = "2021"

[lib]
name = "context_manager"
path = "src/lib.rs"

[[bin]]
name = "context-managerd"
path = "src/bin/context-managerd.rs"

[[bin]]
name = "cm-hook"
path = "src/bin/cm-hook.rs"

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.8"
chrono = { version = "0.4", features = ["serde"] }
anyhow = "1"
clap = { version = "4", features = ["derive"] }
directories = "5"

[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 2: Create a minimal `src/lib.rs`**

```rust
pub mod config;
pub mod usage;
pub mod model_window;
pub mod transcript;
pub mod registration;
pub mod decision;
pub mod monitor;
pub mod tmux;
pub mod lineage;
pub mod handoff;
pub mod paths;
pub mod daemon;
```

> The modules don't exist yet; this file will not compile until later tasks. To keep the scaffold compiling, temporarily comment out every `pub mod` line, then uncomment each as its task lands.

For this task, start with all lines commented:

```rust
// pub mod config;
// pub mod usage;
// pub mod model_window;
// pub mod transcript;
// pub mod registration;
// pub mod decision;
// pub mod monitor;
// pub mod tmux;
// pub mod lineage;
// pub mod handoff;
// pub mod paths;
// pub mod daemon;
```

- [ ] **Step 3: Create placeholder binaries**

`src/bin/context-managerd.rs`:

```rust
fn main() {
    println!("context-managerd placeholder");
}
```

`src/bin/cm-hook.rs`:

```rust
fn main() {
    println!("cm-hook placeholder");
}
```

- [ ] **Step 4: Verify it builds**

Run: `cargo build`
Expected: compiles, produces `target/debug/context-managerd` and `target/debug/cm-hook`.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml src/
git commit -m "feat: scaffold context-manager crate with two binaries"
```

---

## Task 2: Config module

**Files:**
- Create: `src/config.rs`
- Modify: `src/lib.rs` (uncomment `pub mod config;`)

- [ ] **Step 1: Write the failing test**

Add to the bottom of `src/config.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let c = Config::default();
        assert_eq!(c.threshold, 0.50);
        assert_eq!(c.quiet_period_secs, 45);
        assert_eq!(c.grace_secs, 10);
        assert_eq!(c.cooldown_secs, 120);
        assert_eq!(c.poll_interval_secs, 3);
        assert_eq!(c.handoff_timeout_secs, 180);
        assert!(!c.dry_run);
        assert_eq!(c.model_windows.default, 200_000);
    }

    #[test]
    fn loads_partial_toml_and_fills_defaults() {
        let toml_str = r#"
            threshold = 0.40
            dry_run = true
            [model_windows]
            default = 1000000
            "claude-opus-4-8" = 200000
        "#;
        let c = Config::from_toml_str(toml_str).unwrap();
        assert_eq!(c.threshold, 0.40);
        assert!(c.dry_run);
        assert_eq!(c.quiet_period_secs, 45); // default preserved
        assert_eq!(c.model_windows.default, 1_000_000);
        assert_eq!(c.model_windows.overrides.get("claude-opus-4-8"), Some(&200_000));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib config`
Expected: FAIL — `Config` does not exist (after you uncomment `pub mod config;` in `lib.rs`).

- [ ] **Step 3: Write the implementation**

Top of `src/config.rs`:

```rust
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub threshold: f64,
    pub quiet_period_secs: u64,
    pub grace_secs: u64,
    pub cooldown_secs: u64,
    pub poll_interval_secs: u64,
    pub handoff_timeout_secs: u64,
    pub dry_run: bool,
    pub model_windows: ModelWindows,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ModelWindows {
    pub default: u64,
    #[serde(flatten)]
    pub overrides: HashMap<String, u64>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            threshold: 0.50,
            quiet_period_secs: 45,
            grace_secs: 10,
            cooldown_secs: 120,
            poll_interval_secs: 3,
            handoff_timeout_secs: 180,
            dry_run: false,
            model_windows: ModelWindows::default(),
        }
    }
}

impl Default for ModelWindows {
    fn default() -> Self {
        ModelWindows { default: 200_000, overrides: HashMap::new() }
    }
}

impl Config {
    pub fn from_toml_str(s: &str) -> anyhow::Result<Config> {
        Ok(toml::from_str(s)?)
    }

    pub fn load(path: &Path) -> anyhow::Result<Config> {
        if !path.exists() {
            return Ok(Config::default());
        }
        let text = std::fs::read_to_string(path)?;
        Config::from_toml_str(&text)
    }
}
```

Uncomment `pub mod config;` in `src/lib.rs`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib config`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add src/config.rs src/lib.rs
git commit -m "feat: config module with TOML load and defaults"
```

---

## Task 3: Usage parsing

**Files:**
- Create: `src/usage.rs`
- Modify: `src/lib.rs` (uncomment `pub mod usage;`)

- [ ] **Step 1: Write the failing test**

Add to `src/usage.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_tokens_sums_input_and_cache() {
        let u = Usage {
            input_tokens: 6,
            cache_creation_input_tokens: 4101,
            cache_read_input_tokens: 300_534,
            output_tokens: 126,
        };
        assert_eq!(effective_context_tokens(&u), 304_641);
    }

    #[test]
    fn parses_assistant_line_with_usage() {
        let line = r#"{"type":"assistant","message":{"model":"claude-opus-4-8","usage":{"input_tokens":6,"cache_creation_input_tokens":4101,"cache_read_input_tokens":300534,"output_tokens":126}}}"#;
        let parsed = parse_usage_from_line(line).unwrap();
        assert_eq!(parsed.0.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(effective_context_tokens(&parsed.1), 304_641);
    }

    #[test]
    fn ignores_non_assistant_lines() {
        assert!(parse_usage_from_line(r#"{"type":"user","message":{}}"#).is_none());
        assert!(parse_usage_from_line(r#"{"type":"file-history-snapshot"}"#).is_none());
        assert!(parse_usage_from_line("not json").is_none());
    }

    #[test]
    fn missing_usage_fields_default_to_zero() {
        let line = r#"{"type":"assistant","message":{"model":"m","usage":{"output_tokens":5}}}"#;
        let (_model, u) = parse_usage_from_line(line).unwrap();
        assert_eq!(effective_context_tokens(&u), 0);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib usage`
Expected: FAIL — `Usage` not defined.

- [ ] **Step 3: Write the implementation**

Top of `src/usage.rs`:

```rust
use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
}

pub fn effective_context_tokens(u: &Usage) -> u64 {
    u.input_tokens + u.cache_creation_input_tokens + u.cache_read_input_tokens
}

#[derive(Deserialize)]
struct Line {
    #[serde(default)]
    r#type: String,
    #[serde(default)]
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    usage: Option<Usage>,
}

/// Returns (model, usage) when the line is an assistant turn carrying usage.
pub fn parse_usage_from_line(line: &str) -> Option<(Option<String>, Usage)> {
    let parsed: Line = serde_json::from_str(line).ok()?;
    if parsed.r#type != "assistant" {
        return None;
    }
    let msg = parsed.message?;
    let usage = msg.usage?;
    Some((msg.model, usage))
}
```

Uncomment `pub mod usage;` in `src/lib.rs`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib usage`
Expected: PASS (4 tests).

- [ ] **Step 5: Commit**

```bash
git add src/usage.rs src/lib.rs
git commit -m "feat: parse assistant usage and compute effective context tokens"
```

---

## Task 4: Model window resolution

**Files:**
- Create: `src/model_window.rs`
- Modify: `src/lib.rs` (uncomment `pub mod model_window;`)

- [ ] **Step 1: Write the failing test**

Add to `src/model_window.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn config_with_override() -> Config {
        let toml_str = r#"
            [model_windows]
            default = 200000
            "claude-opus-4-8" = 1000000
        "#;
        Config::from_toml_str(toml_str).unwrap()
    }

    #[test]
    fn uses_override_when_present() {
        let c = config_with_override();
        assert_eq!(resolve_window(Some("claude-opus-4-8"), &c), 1_000_000);
    }

    #[test]
    fn falls_back_to_default_for_unknown_model() {
        let c = config_with_override();
        assert_eq!(resolve_window(Some("some-future-model"), &c), 200_000);
    }

    #[test]
    fn falls_back_to_default_for_missing_model() {
        let c = config_with_override();
        assert_eq!(resolve_window(None, &c), 200_000);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib model_window`
Expected: FAIL — `resolve_window` not defined.

- [ ] **Step 3: Write the implementation**

Top of `src/model_window.rs`:

```rust
use crate::config::Config;

/// Resolve the context window (in tokens) for a model id, falling back to the
/// configured default for unknown or missing models.
pub fn resolve_window(model: Option<&str>, config: &Config) -> u64 {
    if let Some(m) = model {
        if let Some(w) = config.model_windows.overrides.get(m) {
            return *w;
        }
    }
    config.model_windows.default
}
```

Uncomment `pub mod model_window;` in `src/lib.rs`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib model_window`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add src/model_window.rs src/lib.rs
git commit -m "feat: resolve model context window with override + default"
```

---

## Task 5: Transcript analysis

**Files:**
- Create: `src/transcript.rs`
- Create: `tests/fixtures/sample.jsonl`
- Modify: `src/lib.rs` (uncomment `pub mod transcript;`)

- [ ] **Step 1: Create the fixture transcript**

`tests/fixtures/sample.jsonl` (each line is one JSON object; last meaningful entry is an assistant turn):

```
{"type":"file-history-snapshot"}
{"type":"user","message":{"role":"user"}}
{"type":"assistant","message":{"model":"claude-opus-4-8","usage":{"input_tokens":10,"cache_creation_input_tokens":1000,"cache_read_input_tokens":50000,"output_tokens":40}}}
{"type":"user","message":{"role":"user"}}
{"type":"assistant","message":{"model":"claude-opus-4-8","usage":{"input_tokens":6,"cache_creation_input_tokens":2000,"cache_read_input_tokens":120000,"output_tokens":80}}}
```

- [ ] **Step 2: Write the failing test**

Add to `src/transcript.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn analyzes_latest_usage_and_last_entry() {
        let state = analyze(Path::new("tests/fixtures/sample.jsonl")).unwrap();
        // latest assistant usage: 6 + 2000 + 120000
        assert_eq!(state.context_tokens, 122_006);
        assert_eq!(state.model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(state.last_entry, EntryKind::Assistant);
    }

    #[test]
    fn empty_file_is_empty_state() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("empty.jsonl");
        std::fs::write(&p, "").unwrap();
        let state = analyze(&p).unwrap();
        assert_eq!(state.context_tokens, 0);
        assert_eq!(state.last_entry, EntryKind::Other);
        assert!(state.model.is_none());
    }

    #[test]
    fn last_entry_user_when_user_turn_is_last() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.jsonl");
        std::fs::write(&p, "{\"type\":\"assistant\",\"message\":{\"model\":\"m\",\"usage\":{\"input_tokens\":1}}}\n{\"type\":\"user\",\"message\":{}}\n").unwrap();
        let state = analyze(&p).unwrap();
        assert_eq!(state.last_entry, EntryKind::User);
        assert_eq!(state.context_tokens, 1);
    }
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test --lib transcript`
Expected: FAIL — `analyze` / `EntryKind` not defined.

- [ ] **Step 4: Write the implementation**

Top of `src/transcript.rs`:

```rust
use crate::usage::{effective_context_tokens, parse_usage_from_line};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Assistant,
    User,
    Other,
}

#[derive(Debug, Clone)]
pub struct TranscriptState {
    pub context_tokens: u64,
    pub model: Option<String>,
    pub last_entry: EntryKind,
}

#[derive(Deserialize)]
struct TypeOnly {
    #[serde(default)]
    r#type: String,
}

/// Read a transcript and report the latest assistant context size, its model,
/// and the kind of the last meaningful (assistant/user) entry. Bookkeeping
/// lines (snapshots, mode changes) are ignored for last-entry classification.
///
/// NOTE: re-reads the whole file each call. Fine for v1 (small files, few
/// sessions, multi-second poll). Tracking a byte offset is a future optimization.
pub fn analyze(path: &Path) -> anyhow::Result<TranscriptState> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(TranscriptState { context_tokens: 0, model: None, last_entry: EntryKind::Other });
        }
        Err(e) => return Err(e.into()),
    };

    let mut context_tokens = 0u64;
    let mut model: Option<String> = None;
    let mut last_entry = EntryKind::Other;

    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Some((m, usage)) = parse_usage_from_line(line) {
            context_tokens = effective_context_tokens(&usage);
            model = m;
        }
        if let Ok(t) = serde_json::from_str::<TypeOnly>(line) {
            match t.r#type.as_str() {
                "assistant" => last_entry = EntryKind::Assistant,
                "user" => last_entry = EntryKind::User,
                _ => {}
            }
        }
    }

    Ok(TranscriptState { context_tokens, model, last_entry })
}
```

Uncomment `pub mod transcript;` in `src/lib.rs`.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib transcript`
Expected: PASS (3 tests).

- [ ] **Step 6: Commit**

```bash
git add src/transcript.rs tests/fixtures/sample.jsonl src/lib.rs
git commit -m "feat: analyze transcript for context size and last-entry kind"
```

---

## Task 6: Registration format + dir scanning

**Files:**
- Create: `src/registration.rs`
- Modify: `src/lib.rs` (uncomment `pub mod registration;`)

- [ ] **Step 1: Write the failing test**

Add to `src/registration.rs`:

```rust
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib registration`
Expected: FAIL — `Registration` not defined.

- [ ] **Step 3: Write the implementation**

Top of `src/registration.rs`:

```rust
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
```

Uncomment `pub mod registration;` in `src/lib.rs`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib registration`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add src/registration.rs src/lib.rs
git commit -m "feat: registration record format with scan/write/remove"
```

---

## Task 7: Eligibility decision logic

**Files:**
- Create: `src/decision.rs`
- Modify: `src/lib.rs` (uncomment `pub mod decision;`)

- [ ] **Step 1: Write the failing test**

Add to `src/decision.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::EntryKind;

    fn inputs() -> EligibilityInputs {
        EligibilityInputs {
            context_pct: 0.60,
            threshold: 0.50,
            last_entry: EntryKind::Assistant,
            quiet_elapsed_secs: 60,
            quiet_period_secs: 45,
            cooldown_active: false,
        }
    }

    #[test]
    fn eligible_when_all_conditions_met() {
        assert!(eligible_for_handoff(&inputs()));
    }

    #[test]
    fn not_eligible_below_threshold() {
        let mut i = inputs();
        i.context_pct = 0.49;
        assert!(!eligible_for_handoff(&i));
    }

    #[test]
    fn not_eligible_when_last_entry_not_assistant() {
        let mut i = inputs();
        i.last_entry = EntryKind::User;
        assert!(!eligible_for_handoff(&i));
    }

    #[test]
    fn not_eligible_when_not_quiet_long_enough() {
        let mut i = inputs();
        i.quiet_elapsed_secs = 10;
        assert!(!eligible_for_handoff(&i));
    }

    #[test]
    fn not_eligible_during_cooldown() {
        let mut i = inputs();
        i.cooldown_active = true;
        assert!(!eligible_for_handoff(&i));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib decision`
Expected: FAIL — `eligible_for_handoff` not defined.

- [ ] **Step 3: Write the implementation**

Top of `src/decision.rs`:

```rust
use crate::transcript::EntryKind;

pub struct EligibilityInputs {
    pub context_pct: f64,
    pub threshold: f64,
    pub last_entry: EntryKind,
    pub quiet_elapsed_secs: u64,
    pub quiet_period_secs: u64,
    pub cooldown_active: bool,
}

/// A session is eligible to begin the handoff flow only when it is over
/// threshold, sitting at a completed assistant turn, has been quiet long
/// enough, and is not in post-handoff cooldown.
pub fn eligible_for_handoff(i: &EligibilityInputs) -> bool {
    i.context_pct >= i.threshold
        && i.last_entry == EntryKind::Assistant
        && i.quiet_elapsed_secs >= i.quiet_period_secs
        && !i.cooldown_active
}
```

Uncomment `pub mod decision;` in `src/lib.rs`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib decision`
Expected: PASS (5 tests).

- [ ] **Step 5: Commit**

```bash
git add src/decision.rs src/lib.rs
git commit -m "feat: pure eligibility decision for handoff"
```

---

## Task 8: SessionMonitor state machine

**Files:**
- Create: `src/monitor.rs`
- Modify: `src/lib.rs` (uncomment `pub mod monitor;`)

The monitor tracks per-session timing state across ticks. The clock is injected
(`now: Instant`) so transitions are testable without sleeping.

- [ ] **Step 1: Write the failing test**

Add to `src/monitor.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::EntryKind;
    use std::time::{Duration, Instant};

    fn over_threshold_idle() -> TickInput {
        TickInput {
            context_pct: 0.60,
            threshold: 0.50,
            last_entry: EntryKind::Assistant,
            transcript_changed: false,
            quiet_period_secs: 45,
            grace_secs: 10,
            cooldown_secs: 120,
        }
    }

    #[test]
    fn activity_resets_quiet_clock_and_holds() {
        let t0 = Instant::now();
        let mut m = SessionMonitor::new(t0);
        // Active turn: transcript changing, not eligible yet.
        let mut input = over_threshold_idle();
        input.transcript_changed = true;
        assert_eq!(m.tick(t0, &input), TickOutcome::Idle);
    }

    #[test]
    fn begins_grace_after_quiet_period() {
        let t0 = Instant::now();
        let mut m = SessionMonitor::new(t0);
        // First observe quiescence start at t0 (no change).
        assert_eq!(m.tick(t0, &over_threshold_idle()), TickOutcome::Idle);
        // 50s later, still quiet and over threshold -> begin grace.
        let t1 = t0 + Duration::from_secs(50);
        assert_eq!(m.tick(t1, &over_threshold_idle()), TickOutcome::NotifyGrace);
    }

    #[test]
    fn executes_after_grace_elapses() {
        let t0 = Instant::now();
        let mut m = SessionMonitor::new(t0);
        m.tick(t0, &over_threshold_idle());
        let t1 = t0 + Duration::from_secs(50);
        assert_eq!(m.tick(t1, &over_threshold_idle()), TickOutcome::NotifyGrace);
        // 11s after grace began -> execute.
        let t2 = t1 + Duration::from_secs(11);
        assert_eq!(m.tick(t2, &over_threshold_idle()), TickOutcome::ExecuteHandoff);
    }

    #[test]
    fn activity_during_grace_cancels() {
        let t0 = Instant::now();
        let mut m = SessionMonitor::new(t0);
        m.tick(t0, &over_threshold_idle());
        let t1 = t0 + Duration::from_secs(50);
        assert_eq!(m.tick(t1, &over_threshold_idle()), TickOutcome::NotifyGrace);
        // User typed: transcript changed during grace -> cancel.
        let t2 = t1 + Duration::from_secs(2);
        let mut input = over_threshold_idle();
        input.transcript_changed = true;
        assert_eq!(m.tick(t2, &input), TickOutcome::CancelGrace);
    }

    #[test]
    fn cooldown_blocks_re_trigger() {
        let t0 = Instant::now();
        let mut m = SessionMonitor::new(t0);
        m.note_handoff_done(t0);
        let t1 = t0 + Duration::from_secs(50);
        // Quiet + over threshold but in cooldown -> Idle.
        assert_eq!(m.tick(t1, &over_threshold_idle()), TickOutcome::Idle);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib monitor`
Expected: FAIL — `SessionMonitor` not defined.

- [ ] **Step 3: Write the implementation**

Top of `src/monitor.rs`:

```rust
use crate::decision::{eligible_for_handoff, EligibilityInputs};
use crate::transcript::EntryKind;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickOutcome {
    Idle,
    NotifyGrace,
    ExecuteHandoff,
    CancelGrace,
}

pub struct TickInput {
    pub context_pct: f64,
    pub threshold: f64,
    pub last_entry: EntryKind,
    /// True if the transcript was written since the previous tick.
    pub transcript_changed: bool,
    pub quiet_period_secs: u64,
    pub grace_secs: u64,
    pub cooldown_secs: u64,
}

pub struct SessionMonitor {
    /// When the current quiet stretch began (reset on any transcript change).
    quiet_since: Instant,
    /// Set when the grace notice has been sent; None otherwise.
    grace_started: Option<Instant>,
    /// Handoffs are suppressed until this time.
    cooldown_until: Option<Instant>,
}

impl SessionMonitor {
    pub fn new(now: Instant) -> Self {
        SessionMonitor { quiet_since: now, grace_started: None, cooldown_until: None }
    }

    pub fn note_handoff_done(&mut self, now: Instant) {
        self.grace_started = None;
        // cooldown is applied relative to the configured duration at tick time;
        // store the moment so tick() can compare. We mark a sentinel here and
        // let the next tick compute the deadline; simpler: store now and treat
        // cooldown as active while (now - cooldown_anchor) < cooldown_secs.
        self.cooldown_until = Some(now);
    }

    fn cooldown_active(&self, now: Instant, cooldown_secs: u64) -> bool {
        match self.cooldown_until {
            Some(anchor) => now.duration_since(anchor).as_secs() < cooldown_secs,
            None => false,
        }
    }

    pub fn tick(&mut self, now: Instant, input: &TickInput) -> TickOutcome {
        if input.transcript_changed {
            self.quiet_since = now;
            if self.grace_started.take().is_some() {
                return TickOutcome::CancelGrace;
            }
            return TickOutcome::Idle;
        }

        let quiet_elapsed_secs = now.duration_since(self.quiet_since).as_secs();
        let cooldown_active = self.cooldown_active(now, input.cooldown_secs);

        // Already counting down a grace window?
        if let Some(started) = self.grace_started {
            if now.duration_since(started).as_secs() >= input.grace_secs {
                return TickOutcome::ExecuteHandoff;
            }
            return TickOutcome::Idle;
        }

        let eligible = eligible_for_handoff(&EligibilityInputs {
            context_pct: input.context_pct,
            threshold: input.threshold,
            last_entry: input.last_entry,
            quiet_elapsed_secs,
            quiet_period_secs: input.quiet_period_secs,
            cooldown_active,
        });

        if eligible {
            self.grace_started = Some(now);
            TickOutcome::NotifyGrace
        } else {
            TickOutcome::Idle
        }
    }
}
```

Uncomment `pub mod monitor;` in `src/lib.rs`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib monitor`
Expected: PASS (5 tests).

- [ ] **Step 5: Commit**

```bash
git add src/monitor.rs src/lib.rs
git commit -m "feat: per-session monitor state machine with injected clock"
```

---

## Task 9: TmuxControl trait + real and fake implementations

**Files:**
- Create: `src/tmux.rs`
- Modify: `src/lib.rs` (uncomment `pub mod tmux;`)

- [ ] **Step 1: Write the failing test**

Add to `src/tmux.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_records_calls() {
        let fake = FakeTmux::new();
        fake.send_text("%1", "hello").unwrap();
        fake.send_enter("%1").unwrap();
        fake.respawn("%1", "claude \"go\"").unwrap();
        let calls = fake.calls();
        assert_eq!(calls[0], "send_text:%1:hello");
        assert_eq!(calls[1], "send_enter:%1");
        assert_eq!(calls[2], "respawn:%1:claude \"go\"");
    }

    #[test]
    fn fake_pane_alive_default_true() {
        let fake = FakeTmux::new();
        assert!(fake.pane_alive("%1").unwrap());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib tmux`
Expected: FAIL — `FakeTmux` not defined.

- [ ] **Step 3: Write the implementation**

Top of `src/tmux.rs`:

```rust
use anyhow::{bail, Context};
use std::process::Command;
use std::sync::Mutex;

pub trait TmuxControl {
    fn send_text(&self, pane: &str, text: &str) -> anyhow::Result<()>;
    fn send_enter(&self, pane: &str) -> anyhow::Result<()>;
    fn respawn(&self, pane: &str, command: &str) -> anyhow::Result<()>;
    fn pane_alive(&self, pane: &str) -> anyhow::Result<bool>;
}

pub struct RealTmux;

impl RealTmux {
    fn run(args: &[&str]) -> anyhow::Result<std::process::Output> {
        let out = Command::new("tmux").args(args).output().context("spawning tmux")?;
        Ok(out)
    }
}

impl TmuxControl for RealTmux {
    fn send_text(&self, pane: &str, text: &str) -> anyhow::Result<()> {
        // `-l` sends text literally (no key-name interpretation).
        let out = RealTmux::run(&["send-keys", "-t", pane, "-l", text])?;
        if !out.status.success() {
            bail!("tmux send-keys -l failed for pane {pane}");
        }
        Ok(())
    }

    fn send_enter(&self, pane: &str) -> anyhow::Result<()> {
        let out = RealTmux::run(&["send-keys", "-t", pane, "Enter"])?;
        if !out.status.success() {
            bail!("tmux send-keys Enter failed for pane {pane}");
        }
        Ok(())
    }

    fn respawn(&self, pane: &str, command: &str) -> anyhow::Result<()> {
        // -k kills the existing pane process before launching the new command.
        let out = RealTmux::run(&["respawn-pane", "-k", "-t", pane, command])?;
        if !out.status.success() {
            bail!("tmux respawn-pane failed for pane {pane}");
        }
        Ok(())
    }

    fn pane_alive(&self, pane: &str) -> anyhow::Result<bool> {
        let out = RealTmux::run(&["list-panes", "-a", "-F", "#{pane_id}"])?;
        if !out.status.success() {
            return Ok(false);
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        Ok(stdout.lines().any(|l| l.trim() == pane))
    }
}

pub struct FakeTmux {
    calls: Mutex<Vec<String>>,
}

impl FakeTmux {
    pub fn new() -> Self {
        FakeTmux { calls: Mutex::new(Vec::new()) }
    }
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl TmuxControl for FakeTmux {
    fn send_text(&self, pane: &str, text: &str) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push(format!("send_text:{pane}:{text}"));
        Ok(())
    }
    fn send_enter(&self, pane: &str) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push(format!("send_enter:{pane}"));
        Ok(())
    }
    fn respawn(&self, pane: &str, command: &str) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push(format!("respawn:{pane}:{command}"));
        Ok(())
    }
    fn pane_alive(&self, _pane: &str) -> anyhow::Result<bool> {
        Ok(true)
    }
}
```

Uncomment `pub mod tmux;` in `src/lib.rs`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib tmux`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add src/tmux.rs src/lib.rs
git commit -m "feat: TmuxControl trait with real and fake implementations"
```

---

## Task 10: Lineage log

**Files:**
- Create: `src/lineage.rs`
- Modify: `src/lib.rs` (uncomment `pub mod lineage;`)

- [ ] **Step 1: Write the failing test**

Add to `src/lineage.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_one_json_line_per_record() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("lineage.jsonl");
        let rec = LineageRecord {
            ts: "2026-06-15T12:00:00Z".into(),
            from_session: "sess-1".into(),
            to_pane: "%3".into(),
            handoff_path: "/tmp/h.md".into(),
            context_pct: 0.52,
            dry_run: false,
        };
        append(&log, &rec).unwrap();
        append(&log, &rec).unwrap();
        let text = std::fs::read_to_string(&log).unwrap();
        assert_eq!(text.lines().count(), 2);
        // each line must parse back as a record
        let first: LineageRecord = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(first.from_session, "sess-1");
        assert_eq!(first.context_pct, 0.52);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib lineage`
Expected: FAIL — `LineageRecord` not defined.

- [ ] **Step 3: Write the implementation**

Top of `src/lineage.rs`:

```rust
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LineageRecord {
    pub ts: String,
    pub from_session: String,
    pub to_pane: String,
    pub handoff_path: String,
    pub context_pct: f64,
    pub dry_run: bool,
}

pub fn append(log_path: &Path, rec: &LineageRecord) -> anyhow::Result<()> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let line = serde_json::to_string(rec)?;
    let mut f = OpenOptions::new().create(true).append(true).open(log_path)?;
    writeln!(f, "{line}")?;
    Ok(())
}
```

Uncomment `pub mod lineage;` in `src/lib.rs`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib lineage`
Expected: PASS (1 test).

- [ ] **Step 5: Commit**

```bash
git add src/lineage.rs src/lib.rs
git commit -m "feat: append-only lineage log"
```

---

## Task 11: XDG path helpers

**Files:**
- Create: `src/paths.rs`
- Modify: `src/lib.rs` (uncomment `pub mod paths;`)

- [ ] **Step 1: Write the failing test**

Add to `src/paths.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_subpaths_from_a_base() {
        let p = Paths::with_base("/home/u/.config/context-manager".into(), "/home/u/.local/share/context-manager".into());
        assert_eq!(p.config_file(), std::path::Path::new("/home/u/.config/context-manager/config.toml"));
        assert_eq!(p.sessions_dir(), std::path::Path::new("/home/u/.local/share/context-manager/sessions"));
        assert_eq!(p.handoff_dir(), std::path::Path::new("/home/u/.local/share/context-manager/handoffs"));
        assert_eq!(p.lineage_file(), std::path::Path::new("/home/u/.local/share/context-manager/lineage.jsonl"));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib paths`
Expected: FAIL — `Paths` not defined.

- [ ] **Step 3: Write the implementation**

Top of `src/paths.rs`:

```rust
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
```

> The test calls `config_file()` etc. and compares to `Path`. `PathBuf` compares equal to `Path` via `==`, so `assert_eq!(p.config_file(), Path::new("..."))` works because `PathBuf: PartialEq<Path>`. If the compiler complains, wrap the expected side in `PathBuf::from(...)`.

Uncomment `pub mod paths;` in `src/lib.rs`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib paths`
Expected: PASS (1 test).

- [ ] **Step 5: Commit**

```bash
git add src/paths.rs src/lib.rs
git commit -m "feat: XDG path helpers for config and state"
```

---

## Task 12: Handoff orchestration

**Files:**
- Create: `src/handoff.rs`
- Modify: `src/lib.rs` (uncomment `pub mod handoff;`)

Orchestrates the swap using a `&dyn TmuxControl`. The "wait for handoff file"
step polls for existence + size stability with a bounded timeout; a `sleep_fn`
is injected so tests don't actually sleep.

- [ ] **Step 1: Write the failing test**

Add to `src/handoff.rs`:

```rust
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib handoff`
Expected: FAIL — `perform_handoff` not defined.

- [ ] **Step 3: Write the implementation**

Top of `src/handoff.rs`:

```rust
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
```

Uncomment `pub mod handoff;` in `src/lib.rs`.

> Note on the timeout test: with `timeout_secs = 1` and a no-op sleep, the loop runs while `waited <= 1` (waited = 0, then 1), never finds a stable non-empty file, and bails. Correct.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib handoff`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add src/handoff.rs src/lib.rs
git commit -m "feat: handoff orchestration driving live session + respawn"
```

---

## Task 13: The hook binary (`cm-hook`)

**Files:**
- Modify: `src/bin/cm-hook.rs`

The hook is invoked by Claude Code on `SessionStart` and `SessionEnd`. Claude
passes hook context as JSON on stdin (`session_id`, `transcript_path`, `cwd`,
`hook_event_name`). The tmux pane comes from `$TMUX_PANE`. The state dir comes
from `$CONTEXT_MANAGER_SESSIONS_DIR` (set by the hook wiring) or the XDG default.

- [ ] **Step 1: Write the implementation**

Replace `src/bin/cm-hook.rs` with:

```rust
use anyhow::Context;
use context_manager::paths::Paths;
use context_manager::registration::{self, Registration};
use serde::Deserialize;
use std::io::Read;

#[derive(Deserialize)]
struct HookInput {
    #[serde(default)]
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

    // SessionEnd: deregister and exit.
    if input.hook_event_name.eq_ignore_ascii_case("SessionEnd") {
        registration::remove(&dir, &input.session_id)?;
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
```

> `pid` here is the hook process's own pid, not Claude's — it is recorded for
> diagnostics only; the daemon keys off `tmux_pane` and `transcript_path`, never pid.

- [ ] **Step 2: Build**

Run: `cargo build`
Expected: compiles.

- [ ] **Step 3: Manual test — register**

```bash
mkdir -p /tmp/cm-test-sessions
echo '{"session_id":"abc","transcript_path":"/tmp/abc.jsonl","cwd":"/tmp","hook_event_name":"SessionStart"}' \
  | TMUX_PANE=%9 CONTEXT_MANAGER_SESSIONS_DIR=/tmp/cm-test-sessions ./target/debug/cm-hook
cat /tmp/cm-test-sessions/abc.json
```

Expected: a JSON file with `session_id: "abc"`, `tmux_pane: "%9"`.

- [ ] **Step 4: Manual test — deregister and not-in-tmux**

```bash
# SessionEnd removes it:
echo '{"session_id":"abc","hook_event_name":"SessionEnd"}' \
  | CONTEXT_MANAGER_SESSIONS_DIR=/tmp/cm-test-sessions ./target/debug/cm-hook
ls /tmp/cm-test-sessions/   # abc.json should be gone

# No TMUX_PANE -> writes nothing:
echo '{"session_id":"xyz","transcript_path":"/tmp/xyz.jsonl","cwd":"/tmp","hook_event_name":"SessionStart"}' \
  | CONTEXT_MANAGER_SESSIONS_DIR=/tmp/cm-test-sessions ./target/debug/cm-hook
ls /tmp/cm-test-sessions/   # xyz.json should NOT exist
```

Expected: as commented.

- [ ] **Step 5: Commit**

```bash
git add src/bin/cm-hook.rs
git commit -m "feat: cm-hook registers/deregisters sessions from hook stdin"
```

---

## Task 14: Daemon loop

**Files:**
- Create: `src/daemon.rs`
- Modify: `src/lib.rs` (uncomment `pub mod daemon;`)

Wires everything: scan registrations, analyze transcripts, track per-session
`SessionMonitor` + transcript mtime, run the state machine, and on
`ExecuteHandoff` either perform the swap or (dry-run) just log.

- [ ] **Step 1: Write the failing test (mtime change detection)**

Add to `src/daemon.rs` (the loop itself is validated by integration + manual
steps; here we unit-test the change detector that decides `transcript_changed`):

```rust
#[cfg(test)]
mod tests {
    use super::*;

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
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib daemon`
Expected: FAIL — `MtimeTracker` not defined.

- [ ] **Step 3: Write the implementation**

Top of `src/daemon.rs`:

```rust
use crate::config::Config;
use crate::handoff::{perform_handoff, HandoffOptions};
use crate::lineage::{self, LineageRecord};
use crate::model_window::resolve_window;
use crate::monitor::{SessionMonitor, TickInput, TickOutcome};
use crate::paths::Paths;
use crate::registration::{self, Registration};
use crate::transcript::{self, EntryKind};
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
        let _ = self.tmux.send_text(&reg.tmux_pane, "");
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
```

Uncomment `pub mod daemon;` in `src/lib.rs`.

> The grace notice (`notify_grace`) is intentionally minimal in v1 — it logs and
> sends an empty literal (a no-op keystroke) rather than risk injecting visible
> text into the user's prompt buffer. A nicer `tmux display-message` popup is a
> future enhancement; keeping it a no-op here avoids polluting the input line.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib daemon`
Expected: PASS (1 test).

- [ ] **Step 5: Commit**

```bash
git add src/daemon.rs src/lib.rs
git commit -m "feat: daemon poll loop wiring detection to handoff"
```

---

## Task 15: Daemon binary entrypoint

**Files:**
- Modify: `src/bin/context-managerd.rs`

- [ ] **Step 1: Write the implementation**

Replace `src/bin/context-managerd.rs` with:

```rust
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
    let daemon = Daemon { config, paths: &paths, tmux: &tmux };

    if args.once {
        let mut monitors = HashMap::new();
        let mut mtimes = Default::default();
        daemon.tick(Instant::now(), &mut monitors, &mut mtimes)?;
        return Ok(());
    }

    daemon.run()
}
```

- [ ] **Step 2: Build**

Run: `cargo build --release`
Expected: compiles; produces `target/release/context-managerd` and `target/release/cm-hook`.

- [ ] **Step 3: Smoke test `--once --dry-run`**

```bash
mkdir -p /tmp/cm-state/sessions
# point XDG state at our temp dir for the smoke test
HOME=/tmp/cm-home XDG_DATA_HOME=/tmp/cm-state XDG_CONFIG_HOME=/tmp/cm-config \
  ./target/release/context-managerd --once --dry-run
```

Expected: prints the startup line and exits cleanly (no sessions registered → no actions). No panic.

- [ ] **Step 4: Commit**

```bash
git add src/bin/context-managerd.rs
git commit -m "feat: daemon binary with --config/--dry-run/--once flags"
```

---

## Task 16: End-to-end dry-run integration test

**Files:**
- Create: `tests/integration_handoff.rs`

This drives the full daemon `tick` against a fake tmux and a synthetic
registration + transcript, asserting a dry-run handoff is decided. It uses the
library API directly (no real tmux, no sleeping) by manipulating time via the
monitor — but since `Daemon::tick` calls `Instant::now()` indirectly through the
monitor, we instead test the decision path by pre-seeding a quiet transcript and
ticking twice with a forced quiet period of 0.

- [ ] **Step 1: Write the test**

`tests/integration_handoff.rs`:

```rust
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
    let mut config = Config::default();
    config.dry_run = true;
    config.quiet_period_secs = 0;
    config.grace_secs = 0;

    let fake = FakeTmux::new();
    let daemon = Daemon { config, paths: &paths, tmux: &fake };

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
```

- [ ] **Step 2: Run the test**

Run: `cargo test --test integration_handoff`
Expected: PASS.

> If tick 2 returns Idle instead of NotifyGrace because mtime equality made the
> baseline tick not count as "changed", verify `MtimeTracker::changed` returns
> `true` on first observation (it does per Task 14). The first tick resets the
> quiet clock to `t0`; with `quiet_period_secs = 0`, `quiet_elapsed_secs (0) >= 0`
> holds on tick 2.

- [ ] **Step 3: Commit**

```bash
git add tests/integration_handoff.rs
git commit -m "test: end-to-end dry-run handoff decision"
```

---

## Task 17: Deployment — hooks wiring + systemd unit + docs

**Files:**
- Create: `deploy/context-manager.service`
- Create: `deploy/install-hooks.md`

- [ ] **Step 1: Create the systemd unit**

`deploy/context-manager.service`:

```ini
[Unit]
Description=Claude Code context manager
After=default.target

[Service]
Type=simple
ExecStart=%h/.local/bin/context-managerd
Restart=always
RestartSec=3
# Keep logs in journald: journalctl --user -u context-manager -f

[Install]
WantedBy=default.target
```

- [ ] **Step 2: Create install docs**

`deploy/install-hooks.md`:

````markdown
# Installing the context manager

## 1. Build and install the binaries

```bash
cargo build --release
mkdir -p ~/.local/bin
cp target/release/context-managerd ~/.local/bin/
cp target/release/cm-hook ~/.local/bin/
```

## 2. Wire the Claude Code hooks

Add to `~/.claude/settings.json` (merge into existing `hooks`):

```json
{
  "hooks": {
    "SessionStart": [
      { "hooks": [ { "type": "command", "command": "~/.local/bin/cm-hook" } ] }
    ],
    "SessionEnd": [
      { "hooks": [ { "type": "command", "command": "~/.local/bin/cm-hook" } ] }
    ]
  }
}
```

Claude passes `session_id`, `transcript_path`, `cwd`, and `hook_event_name` on
stdin; `cm-hook` reads `$TMUX_PANE` from the environment. Sessions started
outside tmux are ignored.

## 3. Create config (optional — defaults are sane)

`~/.config/context-manager/config.toml`:

```toml
threshold = 0.50
quiet_period_secs = 45
grace_secs = 10
dry_run = true          # START IN DRY-RUN; flip to false once validated

[model_windows]
default = 200000
# add 1M-window models explicitly, e.g.:
# "claude-opus-4-8" = 1000000
```

## 4. Install and start the service

```bash
mkdir -p ~/.config/systemd/user
cp deploy/context-manager.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now context-manager
journalctl --user -u context-manager -f
```

## WSL2 caveat

`systemctl --user` requires systemd enabled in `/etc/wsl.conf`:

```ini
[boot]
systemd=true
```

(then `wsl --shutdown` from Windows and reopen). If systemd `--user` is
unavailable, run the daemon under a tmux-resident supervisor instead:

```bash
tmux new-session -d -s context-manager '~/.local/bin/context-managerd'
```
````

- [ ] **Step 3: Validate the unit file syntax**

Run: `systemd-analyze --user verify deploy/context-manager.service` (if available; otherwise visually confirm).
Expected: no errors (warnings about `%h` are fine).

- [ ] **Step 4: Commit**

```bash
git add deploy/
git commit -m "docs: systemd unit and hook installation instructions"
```

---

## Task 18: Live dry-run validation (manual)

**Files:** none (operational validation)

- [ ] **Step 1: Install per `deploy/install-hooks.md` with `dry_run = true` and a low threshold**

Temporarily set `threshold = 0.05` and `quiet_period_secs = 10` in the config so
you can trigger it quickly.

- [ ] **Step 2: Start a real `claude` session inside tmux, do a little work**

In a tmux pane:
```bash
claude
```
Confirm a registration file appeared:
```bash
ls ~/.local/share/context-manager/sessions/
```
Expected: one `<session_id>.json` with the correct `tmux_pane`.

- [ ] **Step 3: Let the session go idle and watch the daemon log**

```bash
journalctl --user -u context-manager -f
```
Expected: after the quiet period, a `DRY-RUN would hand off session ...` line,
and a dry-run record in `~/.local/share/context-manager/lineage.jsonl`. The live
session is untouched.

- [ ] **Step 4: Enable live mode and validate one real handoff**

Set `dry_run = false`, restore sane `threshold`/`quiet_period_secs`, `systemctl --user restart context-manager`.
Drive a session over threshold, let it idle, and confirm:
- the session writes `~/.local/share/context-manager/handoffs/<session_id>.md`,
- the pane respawns into a fresh `claude` seeded with that handoff,
- a non-dry-run lineage record is written.

- [ ] **Step 5: Document the validated thresholds in the config and commit**

```bash
git add -A
git commit -m "chore: validated dry-run and live handoff settings"
```

---

## Self-Review

**Spec coverage:**
- Rust daemon + systemd unit → Tasks 15, 17. ✓
- Auto-adopt via SessionStart hook writing registration → Tasks 6, 13, 17. ✓
- SessionEnd deregistration → Task 13. ✓
- Context measurement (`input + cache_read + cache_creation` / window) → Tasks 3, 4, 5. ✓
- Model→window map with default + overrides + unknown-model fallback → Tasks 2, 4. ✓
- Threshold + safe-point (completed assistant turn + quiet period) → Tasks 7, 8. ✓
- Grace window + cancel-on-activity + cooldown → Task 8. ✓
- Drive live session to write handoff, wait, respawn seeded in same pane → Task 12. ✓
- Lineage logging → Tasks 10, 14. ✓
- Error handling: abort untouched on failure, no retry storms, dry-run → Tasks 12, 14, 15. ✓
- WSL systemd caveat + tmux fallback → Task 17. ✓
- Testing: unit (pure fns), integration (fake tmux), dry-run manual → Tasks 2–12, 16, 18. ✓

**Deviations from spec (intentional, flagged):**
- Polling loop instead of inotify (`notify` crate dropped) — simpler, robust on WSL, latency irrelevant given the quiet period. inotify is a future optimization.
- `respawn-pane -k` kills the old `claude` rather than a graceful `/exit` first — transcript is already persisted, and this is the reliable swap primitive. Documented in Task 12 and the architecture note.
- Grace "notice" is a logged no-op keystroke in v1 rather than visible pane text, to avoid polluting the user's input buffer (Task 14 note).

**Placeholder scan:** none — every code step has complete code; every command has expected output.

**Type consistency:** `Config`, `Usage`/`effective_context_tokens`/`parse_usage_from_line`, `EntryKind`/`TranscriptState`/`analyze`, `Registration`/`scan`/`write`/`remove`, `EligibilityInputs`/`eligible_for_handoff`, `SessionMonitor`/`TickInput`/`TickOutcome`/`note_handoff_done`, `TmuxControl`/`RealTmux`/`FakeTmux`, `LineageRecord`/`append`, `Paths`/`with_base`/`sessions_dir`/`handoff_dir`/`lineage_file`, `HandoffOptions`/`perform_handoff`/`expected_handoff_path`, `Daemon`/`tick`/`run`/`MtimeTracker` — names are used consistently across all tasks.
