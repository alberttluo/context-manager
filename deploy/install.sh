#!/usr/bin/env bash
# context-manager installer — build, install binaries, write config, wire the
# Claude Code hooks, and start the systemd --user service.
#
# Idempotent: safe to re-run. Everything is user-level except enabling linger.
# Usage:  bash deploy/install.sh [--skip-build]
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN_DIR="$HOME/.local/bin"
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/context-manager"
CONFIG_FILE="$CONFIG_DIR/config.toml"
SETTINGS="$HOME/.claude/settings.json"
UNIT_DIR="$HOME/.config/systemd/user"
HOOK_CMD="$BIN_DIR/cm-hook"   # absolute path so it resolves regardless of shell
SKIP_BUILD=0
[ "${1:-}" = "--skip-build" ] && SKIP_BUILD=1

log() { printf '\033[1;34m[install]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[install] ERROR:\033[0m %s\n' "$*" >&2; exit 1; }

# 1. Dependencies ------------------------------------------------------------
command -v jq   >/dev/null || die "jq not found — sudo apt install -y jq"
command -v tmux >/dev/null || log "WARNING: tmux not found — the daemon only manages sessions started inside tmux (sudo apt install -y tmux)"

# 2. Build -------------------------------------------------------------------
# Honour CARGO_TARGET_DIR: cargo writes there instead of $REPO_ROOT/target, so
# assuming the latter would look for binaries that were never put there. This
# matters when the checkout lives somewhere the build output must not — vendored
# inside another repo, or on a volume with a quota.
RELEASE_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}/release"
if [ "$SKIP_BUILD" -eq 0 ]; then
  command -v cargo >/dev/null || die "cargo not found — install Rust (https://rustup.rs) or pass --skip-build"
  log "building release binaries..."
  ( cd "$REPO_ROOT" && cargo build --release )
fi
[ -x "$RELEASE_DIR/context-managerd" ] || die "context-managerd not built at $RELEASE_DIR (drop --skip-build)"
[ -x "$RELEASE_DIR/cm-hook" ]          || die "cm-hook not built at $RELEASE_DIR (drop --skip-build)"

# 3. Install binaries --------------------------------------------------------
mkdir -p "$BIN_DIR"
install -m 0755 "$RELEASE_DIR/context-managerd" "$BIN_DIR/"
install -m 0755 "$RELEASE_DIR/cm-hook"          "$BIN_DIR/"
log "installed binaries -> $BIN_DIR"

# 4. Config (never clobber an existing one) ----------------------------------
mkdir -p "$CONFIG_DIR"
if [ -e "$CONFIG_FILE" ]; then
  log "config exists, leaving as-is: $CONFIG_FILE"
else
  cat > "$CONFIG_FILE" <<'TOML'
threshold = 0.45
quiet_period_secs = 45
grace_secs = 10
cooldown_secs = 120
poll_interval_secs = 3
discovery_interval_secs = 15
handoff_timeout_secs = 180
dry_run = true            # validate first; set false + restart to go live

[model_windows]
default = 200000
"claude-opus-4-8" = 1000000
"claude-opus-4-7" = 1000000   # still inferred — confirm the real window
TOML
  log "wrote default config (dry_run=true): $CONFIG_FILE"
fi

# 5. Wire Claude hooks (idempotent JSON merge) -------------------------------
mkdir -p "$(dirname "$SETTINGS")"
[ -e "$SETTINGS" ] || echo '{}' > "$SETTINGS"
tmp="$(mktemp)"
jq --arg cmd "$HOOK_CMD" '
  def ensure($event):
    .hooks[$event] = ((.hooks[$event] // [])
      | if any(.[]?; any(.hooks[]?; .command == $cmd)) then .
        else . + [ {matcher: "", hooks: [ {type: "command", command: $cmd} ]} ] end);
  .hooks = (.hooks // {}) | ensure("SessionStart") | ensure("SessionEnd")
' "$SETTINGS" > "$tmp" && mv "$tmp" "$SETTINGS"
log "wired SessionStart/SessionEnd hooks -> $SETTINGS"

# 6. systemd --user service --------------------------------------------------
mkdir -p "$UNIT_DIR"
install -m 0644 "$REPO_ROOT/deploy/context-manager.service" "$UNIT_DIR/"
# Linger keeps the --user service alive after you disconnect (essential on a VM).
loginctl enable-linger "$USER" 2>/dev/null \
  || sudo loginctl enable-linger "$USER" 2>/dev/null \
  || log "WARNING: could not enable linger — service will stop when you log out"
systemctl --user daemon-reload
systemctl --user enable --now context-manager
log "service enabled and started"

cat <<EOF

Done.
  Logs:   journalctl --user -u context-manager -f
  Config: $CONFIG_FILE

Validate, then go live:
  1. Start a 'claude' session INSIDE tmux (sessions outside tmux are ignored).
  2. Confirm in the journal: "discovered N new session(s)" and, past threshold,
     "DRY-RUN would hand off ...".
  3. Edit $CONFIG_FILE -> dry_run = false, then:
       systemctl --user restart context-manager
EOF
