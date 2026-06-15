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
    fn respawn(&self, pane: &str, command: &str) -> anyhow::Result<()>;
    fn pane_alive(&self, pane: &str) -> anyhow::Result<bool>;
    fn display_message(&self, pane: &str, msg: &str) -> anyhow::Result<()>;
    fn list_panes(&self) -> anyhow::Result<Vec<PaneInfo>>;
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
}

pub struct FakeTmux {
    calls: Mutex<Vec<String>>,
    panes: Mutex<Vec<PaneInfo>>,
}

impl Default for FakeTmux {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeTmux {
    pub fn new() -> Self {
        FakeTmux { calls: Mutex::new(Vec::new()), panes: Mutex::new(Vec::new()) }
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    pub fn set_panes(&self, panes: Vec<PaneInfo>) {
        *self.panes.lock().unwrap() = panes;
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

    fn display_message(&self, pane: &str, msg: &str) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push(format!("display_message:{pane}:{msg}"));
        Ok(())
    }

    fn list_panes(&self) -> anyhow::Result<Vec<PaneInfo>> {
        Ok(self.panes.lock().unwrap().clone())
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
