#!/usr/bin/env bash
# context-manager installer — build, install binaries, write config, wire the
# Claude Code hooks, and start the background service (systemd --user on Linux,
# a launchd LaunchAgent on macOS).
#
# Idempotent: safe to re-run. Everything is user-level except enabling linger.
# Usage:  bash deploy/install.sh [--skip-build]
set -euo pipefail

OS="$(uname -s)"
case "$OS" in
  Linux|Darwin) ;;
  *) printf 'unsupported OS: %s (Linux and macOS only)\n' "$OS" >&2; exit 1 ;;
esac

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN_DIR="$HOME/.local/bin"
# Must match paths.rs, which uses these XDG locations on macOS too — the native
# ~/Library/Application Support would be a config the daemon never reads.
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/context-manager"
CONFIG_FILE="$CONFIG_DIR/config.toml"
SETTINGS="$HOME/.claude/settings.json"
UNIT_DIR="$HOME/.config/systemd/user"
AGENT_LABEL="com.context-manager.daemon"
AGENT_DIR="$HOME/Library/LaunchAgents"
AGENT_PLIST="$AGENT_DIR/$AGENT_LABEL.plist"
MAC_LOG="$HOME/Library/Logs/context-manager.log"
HOOK_CMD="$BIN_DIR/cm-hook"   # absolute path so it resolves regardless of shell
SKIP_BUILD=0
[ "${1:-}" = "--skip-build" ] && SKIP_BUILD=1

log() { printf '\033[1;34m[install]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[install] ERROR:\033[0m %s\n' "$*" >&2; exit 1; }

# 1. Dependencies ------------------------------------------------------------
if [ "$OS" = Darwin ]; then INSTALL_HINT="brew install"; else INSTALL_HINT="sudo apt install -y"; fi
command -v jq   >/dev/null || die "jq not found — $INSTALL_HINT jq"
command -v tmux >/dev/null || log "WARNING: tmux not found — the daemon only manages sessions started inside tmux ($INSTALL_HINT tmux)"

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

# 6. Background service ------------------------------------------------------
# Linger keeps the --user service alive after you disconnect (essential on a VM).
#
# Root is only needed on hosts where the unprivileged call is refused. Try it
# without sudo, then passwordless sudo, and only then ask for a password — and
# only if there is a terminal to ask on, since an unattended install has nobody
# to type one and would otherwise block forever. The previous `sudo ... 2>/dev/null`
# managed the worst of both: it could still prompt, but with its own error
# output discarded.
enable_linger() {
  loginctl enable-linger "$USER" 2>/dev/null && return 0
  command -v sudo >/dev/null 2>&1 || return 1
  sudo -n loginctl enable-linger "$USER" 2>/dev/null && return 0
  { [ -t 0 ] && [ -t 2 ]; } || return 1
  log "enabling linger needs root — enter your password, or leave it empty to skip"
  # stderr left alone here: a prompt you cannot see is worse than no prompt.
  sudo -v || return 1
  sudo -n loginctl enable-linger "$USER" 2>/dev/null
}

install_service_linux() {
  mkdir -p "$UNIT_DIR"
  install -m 0644 "$REPO_ROOT/deploy/context-manager.service" "$UNIT_DIR/"
  # A missing password is not fatal. Linger is a nicety; without it the daemon
  # simply stops when you log out, which the warning says plainly.
  enable_linger \
    || log "WARNING: could not enable linger — service will stop when you log out"
  systemctl --user daemon-reload
  systemctl --user enable context-manager
  # `enable --now` starts a stopped unit but does nothing to a running one, so an
  # already-running daemon kept executing the binary it started with while this
  # script reported success — the new binaries only took effect at the next
  # reboot. Step 3 just replaced them, so restart unconditionally. `restart` also
  # starts a unit that is not running, which is why it replaces --now outright.
  systemctl --user restart context-manager
  log "service enabled and restarted onto the new binaries"
}

# macOS has no systemd; the equivalent is a per-user LaunchAgent. It needs no
# linger equivalent — launchd starts the agent at login on its own — but it also
# has no journald, so the daemon's stderr is redirected to a log file.
install_service_macos() {
  mkdir -p "$AGENT_DIR" "$(dirname "$MAC_LOG")"

  # tmux is how the daemon reaches sessions, and an agent's inherited PATH does
  # not include Homebrew, so tmux's actual directory is baked into the plist.
  local tmux_bin agent_path="/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
  tmux_bin="$(command -v tmux || true)"
  [ -n "$tmux_bin" ] && agent_path="$(dirname "$tmux_bin"):$agent_path"

  sed -e "s|__BIN__|$BIN_DIR/context-managerd|g" \
      -e "s|__LOG__|$MAC_LOG|g" \
      -e "s|__PATH__|$agent_path|g" \
      "$REPO_ROOT/deploy/$AGENT_LABEL.plist" > "$AGENT_PLIST"

  local uid target
  uid="$(id -u)"
  target="gui/$uid"
  # An ssh-only session has no GUI (Aqua) domain to bootstrap into. Fall back to
  # the legacy load, which uses the calling session's own domain — the daemon
  # then dies with that session, the same caveat linger covers on Linux.
  if ! launchctl print "$target" >/dev/null 2>&1; then
    log "WARNING: no GUI login session — loading into this session instead; the daemon will stop when it ends"
    launchctl unload "$AGENT_PLIST" 2>/dev/null || true
    launchctl load -w "$AGENT_PLIST"
    return
  fi

  # `enable` first: bootstrap refuses a label the user previously disabled.
  launchctl enable "$target/$AGENT_LABEL" 2>/dev/null || true
  # bootout then bootstrap is a full reload. It is also what moves an
  # already-running daemon onto the binaries step 3 just installed — bootstrap
  # alone fails outright on a loaded agent, leaving the old process in place.
  launchctl bootout "$target/$AGENT_LABEL" 2>/dev/null || true
  launchctl bootstrap "$target" "$AGENT_PLIST" \
    || die "launchctl bootstrap failed for $AGENT_PLIST"
  log "LaunchAgent loaded and restarted onto the new binaries"
}

if [ "$OS" = Darwin ]; then install_service_macos; else install_service_linux; fi

if [ "$OS" = Darwin ]; then
  LOGS_CMD="tail -f $MAC_LOG"
  RESTART_CMD="launchctl kickstart -k gui/\$(id -u)/$AGENT_LABEL"
else
  LOGS_CMD="journalctl --user -u context-manager -f"
  RESTART_CMD="systemctl --user restart context-manager"
fi

cat <<EOF

Done.
  Logs:   $LOGS_CMD
  Config: $CONFIG_FILE

Validate, then go live:
  1. Start a 'claude' session INSIDE tmux (sessions outside tmux are ignored).
  2. Confirm in the log: "discovered N new session(s)" and, past threshold,
     "DRY-RUN would hand off ...".
  3. Edit $CONFIG_FILE -> dry_run = false, then:
       $RESTART_CMD
EOF
