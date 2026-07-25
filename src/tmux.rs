use anyhow::{bail, Context};
use std::process::Command;
use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq)]
pub struct PaneInfo {
    pub pane_id: String,
    pub cwd: String,
    pub command: String,
}

pub trait TmuxControl {
    fn send_text(&self, pane: &str, text: &str) -> anyhow::Result<()>;
    fn send_enter(&self, pane: &str) -> anyhow::Result<()>;
    /// Discard whatever is sitting on the current input line (C-u), so a retried
    /// send never concatenates onto a partially-delivered previous attempt.
    fn clear_input(&self, pane: &str) -> anyhow::Result<()>;
    /// The pane's visible text, used to confirm that keystrokes actually landed.
    fn capture_pane(&self, pane: &str) -> anyhow::Result<String>;
    /// The pane's current foreground command (e.g. "claude", "zsh").
    fn pane_command(&self, pane: &str) -> anyhow::Result<String>;
    fn respawn_shell(&self, pane: &str) -> anyhow::Result<()>;
    fn pane_alive(&self, pane: &str) -> anyhow::Result<bool>;
    fn display_message(&self, pane: &str, msg: &str) -> anyhow::Result<()>;
    fn list_panes(&self) -> anyhow::Result<Vec<PaneInfo>>;
    /// The flags the pane's `claude` process was launched with (argv after the
    /// program), so a successor can be started identically. Empty if none can
    /// be determined.
    fn pane_launch_flags(&self, pane: &str) -> anyhow::Result<Vec<String>>;
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

    fn clear_input(&self, pane: &str) -> anyhow::Result<()> {
        let out = RealTmux::run(&["send-keys", "-t", pane, "C-u"])?;
        if !out.status.success() {
            bail!("tmux send-keys C-u failed for pane {pane}");
        }
        Ok(())
    }

    fn capture_pane(&self, pane: &str) -> anyhow::Result<String> {
        let out = RealTmux::run(&["capture-pane", "-p", "-t", pane])?;
        if !out.status.success() {
            bail!("tmux capture-pane failed for pane {pane}");
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn pane_command(&self, pane: &str) -> anyhow::Result<String> {
        let out = RealTmux::run(&["display-message", "-p", "-t", pane, "#{pane_current_command}"])?;
        if !out.status.success() {
            bail!("tmux display-message failed for pane {pane}");
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    fn respawn_shell(&self, pane: &str) -> anyhow::Result<()> {
        // -k kills the existing process; with NO command, tmux starts the
        // default shell as a normal interactive login shell, so .zshrc runs and
        // the full environment (PATH, NODE_EXTRA_CA_CERTS, proxy vars) loads.
        // Launching claude by typing into this shell — rather than
        // `respawn-pane 'claude ...'` which runs via a bare `sh -c` that skips
        // shell init — is what makes the API/certs work. It also means the pane
        // survives if claude later exits: it falls back to the shell prompt
        // instead of the pane (and its window) being destroyed.
        let out = RealTmux::run(&["respawn-pane", "-k", "-t", pane])?;
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

    fn display_message(&self, pane: &str, msg: &str) -> anyhow::Result<()> {
        let out = RealTmux::run(&["display-message", "-t", pane, msg])?;
        if !out.status.success() {
            bail!("tmux display-message failed for pane {pane}");
        }
        Ok(())
    }

    fn list_panes(&self) -> anyhow::Result<Vec<PaneInfo>> {
        let out = RealTmux::run(&[
            "list-panes", "-a", "-F",
            "#{pane_id}\t#{pane_current_path}\t#{pane_current_command}",
        ])?;
        if !out.status.success() {
            bail!("tmux list-panes failed");
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let panes = stdout
            .lines()
            .filter_map(|line| {
                let mut parts = line.splitn(3, '\t');
                let pane_id = parts.next()?.to_string();
                let cwd = parts.next()?.to_string();
                let command = parts.next()?.to_string();
                Some(PaneInfo { pane_id, cwd, command })
            })
            .collect();
        Ok(panes)
    }

    fn pane_launch_flags(&self, pane: &str) -> anyhow::Result<Vec<String>> {
        let out = RealTmux::run(&["display-message", "-p", "-t", pane, "#{pane_pid}"])?;
        if !out.status.success() {
            return Ok(Vec::new());
        }
        let pane_pid: u32 = match String::from_utf8_lossy(&out.stdout).trim().parse() {
            Ok(p) => p,
            Err(_) => return Ok(Vec::new()),
        };
        Ok(claude_launch_flags(pane_pid))
    }
}

/// argv of a process from /proc/<pid>/cmdline (NUL-delimited), or empty.
fn proc_argv(pid: u32) -> Vec<String> {
    match std::fs::read(format!("/proc/{pid}/cmdline")) {
        Ok(bytes) => bytes
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn proc_ppid(pid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("PPid:").and_then(|r| r.trim().parse().ok()))
}

fn argv0_is_claude(argv: &[String]) -> bool {
    argv.first()
        .map(|a| a.rsplit('/').next().unwrap_or(a) == "claude")
        .unwrap_or(false)
}

/// Find the `claude` process at or below `root_pid` (BFS over the process tree)
/// and return its launch flags (argv after the program name). Linux-only
/// (/proc); returns empty if no claude process is found.
fn claude_launch_flags(root_pid: u32) -> Vec<String> {
    use std::collections::{HashMap, HashSet, VecDeque};
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for e in entries.flatten() {
            if let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) {
                if let Some(ppid) = proc_ppid(pid) {
                    children.entry(ppid).or_default().push(pid);
                }
            }
        }
    }
    let mut queue = VecDeque::from([root_pid]);
    let mut seen = HashSet::new();
    while let Some(pid) = queue.pop_front() {
        if !seen.insert(pid) {
            continue;
        }
        let argv = proc_argv(pid);
        if argv0_is_claude(&argv) {
            return argv[1..].to_vec();
        }
        if let Some(kids) = children.get(&pid) {
            queue.extend(kids.iter().copied());
        }
    }
    Vec::new()
}

/// In-memory tmux stand-in that simulates a pane well enough to exercise the
/// verified-send protocol: text accumulates in an "input box", Enter submits it,
/// and `swallow_sends` reproduces the real failure where a TUI that is not yet
/// reading input silently discards keystrokes.
pub struct FakeTmux {
    calls: Mutex<Vec<String>>,
    panes: Mutex<Vec<PaneInfo>>,
    launch_flags: Mutex<Vec<String>>,
    /// Text currently sitting on the pane's input line.
    input_box: Mutex<String>,
    /// Text already submitted/echoed above the input line.
    scrollback: Mutex<String>,
    pane_command: Mutex<String>,
    /// Number of upcoming `send_text` calls to drop on the floor.
    swallow_sends: Mutex<u32>,
    /// Whether submitting a `claude ...` command line actually starts claude.
    successor_starts: Mutex<bool>,
}

impl Default for FakeTmux {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeTmux {
    pub fn new() -> Self {
        FakeTmux {
            calls: Mutex::new(Vec::new()),
            panes: Mutex::new(Vec::new()),
            launch_flags: Mutex::new(Vec::new()),
            input_box: Mutex::new(String::new()),
            scrollback: Mutex::new(String::new()),
            pane_command: Mutex::new("claude".to_string()),
            swallow_sends: Mutex::new(0),
            successor_starts: Mutex::new(true),
        }
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    pub fn set_panes(&self, panes: Vec<PaneInfo>) {
        *self.panes.lock().unwrap() = panes;
    }

    pub fn set_launch_flags(&self, flags: Vec<String>) {
        *self.launch_flags.lock().unwrap() = flags;
    }

    /// Drop the next `n` `send_text` calls, as a not-yet-ready TUI does.
    pub fn swallow_next_sends(&self, n: u32) {
        *self.swallow_sends.lock().unwrap() = n;
    }

    /// When false, typing a `claude ...` line into the shell never brings claude
    /// up — the observed "handoff reported success but no successor" failure.
    pub fn set_successor_starts(&self, yes: bool) {
        *self.successor_starts.lock().unwrap() = yes;
    }

    pub fn set_pane_command(&self, cmd: &str) {
        *self.pane_command.lock().unwrap() = cmd.to_string();
    }
}

impl TmuxControl for FakeTmux {
    fn send_text(&self, pane: &str, text: &str) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push(format!("send_text:{pane}:{text}"));
        let mut swallow = self.swallow_sends.lock().unwrap();
        if *swallow > 0 {
            *swallow -= 1;
            return Ok(()); // keystrokes discarded, exactly as a cold TUI does
        }
        self.input_box.lock().unwrap().push_str(text);
        Ok(())
    }

    fn send_enter(&self, pane: &str) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push(format!("send_enter:{pane}"));
        let submitted = std::mem::take(&mut *self.input_box.lock().unwrap());
        if submitted.is_empty() {
            return Ok(());
        }
        self.scrollback.lock().unwrap().push_str(&format!("{submitted}\n"));
        // A `claude ...` line typed at a shell prompt starts claude in the pane.
        let is_shell = { self.pane_command.lock().unwrap().as_str() != "claude" };
        if is_shell && submitted.trim_start().starts_with("claude") && *self.successor_starts.lock().unwrap() {
            *self.pane_command.lock().unwrap() = "claude".to_string();
        }
        Ok(())
    }

    fn clear_input(&self, pane: &str) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push(format!("clear_input:{pane}"));
        self.input_box.lock().unwrap().clear();
        Ok(())
    }

    fn capture_pane(&self, _pane: &str) -> anyhow::Result<String> {
        // Mirror the real TUI: scrollback, then the input box inside a frame.
        let border = "─".repeat(40);
        Ok(format!(
            "{}\n{border}\n❯ {}\n{border}\n",
            self.scrollback.lock().unwrap(),
            self.input_box.lock().unwrap()
        ))
    }

    fn pane_command(&self, _pane: &str) -> anyhow::Result<String> {
        Ok(self.pane_command.lock().unwrap().clone())
    }

    fn respawn_shell(&self, pane: &str) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push(format!("respawn_shell:{pane}"));
        *self.pane_command.lock().unwrap() = "zsh".to_string();
        self.input_box.lock().unwrap().clear();
        self.scrollback.lock().unwrap().clear();
        Ok(())
    }

    fn pane_alive(&self, _pane: &str) -> anyhow::Result<bool> {
        Ok(true)
    }

    fn display_message(&self, pane: &str, msg: &str) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push(format!("display_message:{pane}:{msg}"));
        Ok(())
    }

    fn list_panes(&self) -> anyhow::Result<Vec<PaneInfo>> {
        Ok(self.panes.lock().unwrap().clone())
    }

    fn pane_launch_flags(&self, _pane: &str) -> anyhow::Result<Vec<String>> {
        Ok(self.launch_flags.lock().unwrap().clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_records_calls() {
        let fake = FakeTmux::new();
        fake.send_text("%1", "hello").unwrap();
        fake.send_enter("%1").unwrap();
        fake.respawn_shell("%1").unwrap();
        let calls = fake.calls();
        assert_eq!(calls[0], "send_text:%1:hello");
        assert_eq!(calls[1], "send_enter:%1");
        assert_eq!(calls[2], "respawn_shell:%1");
    }

    #[test]
    fn fake_pane_alive_default_true() {
        let fake = FakeTmux::new();
        assert!(fake.pane_alive("%1").unwrap());
    }

    #[test]
    fn fake_list_panes_returns_set_panes() {
        let fake = FakeTmux::new();
        assert!(fake.list_panes().unwrap().is_empty());
        let panes = vec![
            PaneInfo { pane_id: "%1".into(), cwd: "/home/user/proj".into(), command: "claude".into() },
            PaneInfo { pane_id: "%2".into(), cwd: "/tmp".into(), command: "bash".into() },
        ];
        fake.set_panes(panes.clone());
        assert_eq!(fake.list_panes().unwrap(), panes);
    }
}
