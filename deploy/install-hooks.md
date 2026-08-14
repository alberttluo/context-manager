# Installing the context manager

Supported on Linux and macOS. `bash deploy/install.sh` does everything below and
picks the right service manager for the platform; the manual steps follow.

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
      { "matcher": "", "hooks": [ { "type": "command", "command": "~/.local/bin/cm-hook" } ] }
    ],
    "SessionEnd": [
      { "matcher": "", "hooks": [ { "type": "command", "command": "~/.local/bin/cm-hook" } ] }
    ]
  }
}
```

Claude passes `session_id`, `transcript_path`, `cwd`, and `hook_event_name` on
stdin; `cm-hook` reads `$TMUX_PANE` from the environment. Sessions started
outside tmux are ignored.

## 3. Create config (optional — defaults are sane)

`~/.config/context-manager/config.toml` — on macOS too, not
`~/Library/Application Support`. State lives in `~/.local/share/context-manager`
on both platforms. (`$XDG_CONFIG_HOME` / `$XDG_DATA_HOME` override both if set to
an absolute path.)

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

### Linux (systemd --user)

```bash
mkdir -p ~/.config/systemd/user
cp deploy/context-manager.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now context-manager
journalctl --user -u context-manager -f
```

### macOS (launchd)

The plist is a template: launchd expands neither `~` nor environment variables,
so substitute the absolute paths in. `__PATH__` must contain the directory tmux
actually lives in — a LaunchAgent inherits a minimal PATH with no Homebrew, and
the daemon reaches sessions by shelling out to tmux.

```bash
LABEL=com.context-manager.daemon
mkdir -p ~/Library/LaunchAgents ~/Library/Logs
sed -e "s|__BIN__|$HOME/.local/bin/context-managerd|g" \
    -e "s|__LOG__|$HOME/Library/Logs/context-manager.log|g" \
    -e "s|__PATH__|$(dirname "$(command -v tmux)"):/usr/bin:/bin:/usr/sbin:/sbin|g" \
    deploy/$LABEL.plist > ~/Library/LaunchAgents/$LABEL.plist

launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true   # no-op if not loaded
launchctl bootstrap "gui/$(id -u)" ~/Library/LaunchAgents/$LABEL.plist
tail -f ~/Library/Logs/context-manager.log
```

There is no journald, hence the log file, and no linger to enable: launchd
starts the agent at login by itself. Reload it after replacing the binaries —
`bootout` then `bootstrap`, or `launchctl kickstart -k gui/$(id -u)/$LABEL` —
since a running daemon otherwise keeps executing the old binary.

Over SSH with nobody logged in at the console there is no `gui/$UID` domain to
bootstrap into; `launchctl load -w ~/Library/LaunchAgents/$LABEL.plist` works
there, but the daemon stops when that session ends.

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
