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
