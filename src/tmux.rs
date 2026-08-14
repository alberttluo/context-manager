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

/// One process as the tree walk needs it. `argv` is empty when the process's
/// command line cannot be read (a permissions failure, or the process exiting
/// between listing and reading).
#[derive(Debug, Clone, PartialEq)]
struct ProcessEntry {
    pid: u32,
    ppid: u32,
    argv: Vec<String>,
}

/// argv of a process from /proc/<pid>/cmdline (NUL-delimited), or empty.
#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
fn proc_ppid(pid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("PPid:").and_then(|r| r.trim().parse().ok()))
}

/// Every process on the machine, read from /proc.
#[cfg(target_os = "linux")]
fn process_table() -> Vec<ProcessEntry> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()))
        .filter_map(|pid| {
            proc_ppid(pid).map(|ppid| ProcessEntry { pid, ppid, argv: proc_argv(pid) })
        })
        .collect()
}

/// Every process on the machine, read from `ps` — the portable source where
/// there is no /proc (macOS, the BSDs).
#[cfg(not(target_os = "linux"))]
fn process_table() -> Vec<ProcessEntry> {
    let out = match Command::new("ps").args(["-Ao", "pid=,ppid=,args="]).output() {
        Ok(out) if out.status.success() => out,
        _ => return Vec::new(),
    };
    parse_ps_table(&String::from_utf8_lossy(&out.stdout))
}

/// Parse `ps -Ao pid=,ppid=,args=` output: two numeric columns then the command
/// line.
///
/// `ps` joins argv with spaces and cannot un-join it, so an argument that itself
/// contains a space arrives split in two. Only the flags a successor is launched
/// with are read from here, and those are shell-quoted again before use, so the
/// damage is confined to an unusual flag value being passed as two arguments —
/// worth accepting for a process list that needs no extra dependency. On Linux,
/// where /proc/<pid>/cmdline delimits argv with NULs, no such split happens.
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn parse_ps_table(stdout: &str) -> Vec<ProcessEntry> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let ppid = fields.next()?.parse().ok()?;
            let argv = fields.map(str::to_string).collect();
            Some(ProcessEntry { pid, ppid, argv })
        })
        .collect()
}

/// Whether a command name (a pane's foreground command, or an argv[0]) is the
/// Claude Code CLI.
///
/// The `.exe` is not a typo and not Windows: Claude Code ships its macOS binary
/// as `bin/claude.exe`, and tmux on macOS derives `pane_current_command` from
/// the executable path, so panes report `claude.exe` there and plain `claude` on
/// Linux. Matching only the bare name silently disables the whole daemon on
/// macOS — no pane is ever recognised as a session.
pub fn is_claude_command(command: &str) -> bool {
    let base = command.rsplit('/').next().unwrap_or(command);
    base.strip_suffix(".exe").unwrap_or(base) == "claude"
}

fn argv0_is_claude(argv: &[String]) -> bool {
    argv.first().map(|a| is_claude_command(a)).unwrap_or(false)
}

/// Find the `claude` process at or below `root_pid` (BFS over the process tree)
/// and return its launch flags (argv after the program name). Empty if no
/// claude process is found.
fn claude_launch_flags(root_pid: u32) -> Vec<String> {
    flags_from_table(&process_table(), root_pid)
}

fn flags_from_table(table: &[ProcessEntry], root_pid: u32) -> Vec<String> {
    use std::collections::{HashMap, HashSet, VecDeque};
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut argv_by_pid: HashMap<u32, &[String]> = HashMap::new();
    for entry in table {
        children.entry(entry.ppid).or_default().push(entry.pid);
        argv_by_pid.insert(entry.pid, &entry.argv);
    }
    let mut queue = VecDeque::from([root_pid]);
    let mut seen = HashSet::new();
    while let Some(pid) = queue.pop_front() {
        if !seen.insert(pid) {
            continue;
        }
        if let Some(argv) = argv_by_pid.get(&pid) {
            if argv0_is_claude(argv) {
                return argv[1..].to_vec();
            }
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
        let is_shell = { !is_claude_command(self.pane_command.lock().unwrap().as_str()) };
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
    fn recognises_claude_under_both_platforms_names() {
        // Linux tmux reports the bare name; macOS tmux reports the executable,
        // which Claude Code ships as claude.exe.
        assert!(is_claude_command("claude"));
        assert!(is_claude_command("claude.exe"));
        assert!(is_claude_command("/opt/homebrew/bin/claude"));
        assert!(is_claude_command(
            "/Users/u/.nvm/versions/node/v24.2.0/lib/node_modules/@anthropic-ai/claude-code/bin/claude.exe"
        ));
        assert!(!is_claude_command("zsh"));
        assert!(!is_claude_command("claude-code"));
        assert!(!is_claude_command("notclaude"));
    }

    #[test]
    fn parses_ps_output_including_a_command_line_with_spaces() {
        let stdout = "\
    1     0 /sbin/launchd
 4210  4207 -zsh
 4288  4210 node /opt/homebrew/bin/claude --model opus
 4290  4288 /bin/sh -c echo hi
  bad line without numbers
";
        let table = parse_ps_table(stdout);
        assert_eq!(table.len(), 4, "expected the unparseable line to be skipped: {table:?}");
        assert_eq!(table[0], ProcessEntry { pid: 1, ppid: 0, argv: vec!["/sbin/launchd".into()] });
        assert_eq!(table[2].pid, 4288);
        assert_eq!(table[2].ppid, 4210);
        assert_eq!(
            table[2].argv,
            vec!["node", "/opt/homebrew/bin/claude", "--model", "opus"],
        );
    }

    /// Exercises the real platform source (/proc or `ps`), which the parser
    /// tests cannot: a wrong `ps` invocation still parses to an empty table.
    #[test]
    fn process_table_sees_this_process() {
        let table = process_table();
        let me = std::process::id();
        assert!(
            table.iter().any(|e| e.pid == me),
            "process table lacks our own pid {me} ({} entries)",
            table.len(),
        );
    }

    fn entry(pid: u32, ppid: u32, argv: &[&str]) -> ProcessEntry {
        ProcessEntry { pid, ppid, argv: argv.iter().map(|s| s.to_string()).collect() }
    }

    #[test]
    fn finds_claude_flags_below_the_pane_process() {
        let table = vec![
            entry(1, 0, &["/sbin/launchd"]),
            entry(10, 1, &["-zsh"]),                                  // the pane
            entry(20, 10, &["/usr/bin/claude", "--model", "opus"]),   // its claude
            entry(30, 1, &["/usr/bin/claude", "--other-session"]),    // unrelated
        ];
        assert_eq!(flags_from_table(&table, 10), vec!["--model", "opus"]);
    }

    #[test]
    fn no_claude_below_the_pane_yields_no_flags() {
        let table = vec![entry(10, 1, &["-zsh"]), entry(20, 10, &["vim"])];
        assert!(flags_from_table(&table, 10).is_empty());
        // Unknown pid, and an empty table, are both simply "nothing found".
        assert!(flags_from_table(&table, 999).is_empty());
        assert!(flags_from_table(&[], 10).is_empty());
    }

    #[test]
    fn a_parent_cycle_does_not_hang_the_walk() {
        // Reparenting races can hand us a table where a pid is its own ancestor.
        let table = vec![entry(10, 20, &["a"]), entry(20, 10, &["b"])];
        assert!(flags_from_table(&table, 10).is_empty());
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
