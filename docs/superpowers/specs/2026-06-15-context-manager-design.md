# Context Manager — Design

**Date:** 2026-06-15
**Status:** Approved (pending implementation plan)

## Summary

A long-running **Rust daemon**, managed as a `systemctl --user` service, that
observes every Claude Code session via its JSONL transcript. When a session's
context crosses a configurable threshold (default 50%, target range 40–50%) *at
a safe idle boundary*, the daemon drives a **fully automatic handoff**: it
instructs the live session to write a handoff document, retires the session, and
respawns a fresh session in the same tmux pane seeded only with that document —
reducing context rot.

Locked decisions from brainstorming:

- **Session type:** interactive sessions the user drives.
- **Automation level:** fully automatic handoff.
- **Adoption model:** auto-adopt — user keeps launching `claude` inside tmux; a
  `SessionStart` hook registers each session with the daemon.
- **Handoff generation:** drive the live session (it writes the doc using its
  full in-context state).
- **Language / process model:** Rust, `systemctl --user` unit.

## Goals

- Detect, with low overhead, when any interactive Claude session exceeds the
  context threshold.
- Hand off automatically without yanking the user out mid-interaction.
- Keep the user in place (same tmux pane) across the swap.
- Never crash, corrupt, or unexpectedly kill a user's session.

## Non-Goals (deferred / YAGNI)

- TUI dashboard for listing/switching sessions.
- Headless summarizer fallback for handoff generation.
- Non-tmux session support.
- Multi-machine coordination.
- History / metrics visualization.

## Architecture

Three pieces:

1. **Registration hook** — a tiny binary or shell stub invoked on
   `SessionStart` / `SessionEnd`.
2. **Daemon** (Rust) — the watch → measure → decide → act loop.
3. **Config + state** — TOML config, a runtime state directory, and a lineage
   log.

### Data flow

```
claude launches (in tmux)
        │  SessionStart hook
        ▼
sessions/<session_id>.json  ──watch──►  Daemon
        ▲                                  │ watch transcript (inotify)
        │                                  │ compute context %
   SessionEnd hook                         │ threshold + safe-point?
        │                                  ▼
        └──────────────  tmux send-keys / respawn-pane  (handoff + swap)
```

## Components

### 1. Session discovery (auto-adopt)

A `SessionStart` hook fires when the user runs `claude` inside tmux. It:

- reads the hook's stdin JSON (`session_id`, `transcript_path`, `cwd`),
- reads `$TMUX_PANE` from the environment,
- writes a registration file to
  `~/.local/share/context-manager/sessions/<session_id>.json` containing
  `{ session_id, transcript_path, cwd, tmux_pane, pid, started_at }`.

A `SessionEnd` hook (plus daemon-side pane-death detection) removes the
registration. The hook supplies the exact transcript path, so the daemon never
guesses path encodings.

### 2. Context measurement

The daemon watches each registered transcript with inotify (`notify` crate). On
each append it parses the new lines and tracks the most recent assistant
`usage` block. Effective context is:

```
input_tokens + cache_read_input_tokens + cache_creation_input_tokens
```

This is divided by the model's context window, taken from a configurable
**model → window map**:

- ships with defaults for known models (opus / sonnet / haiku),
- allows explicit per-model overrides,
- falls back to a conservative default window for unknown models and logs a
  warning.

> A wrong window would mis-trigger handoffs — a real observed session read
> ~304k tokens, so 1M-window models exist and the map must be correct.

### 3. Threshold + safe-point detection

Both conditions must hold before acting:

- **Over threshold** — context % ≥ configured threshold (default 0.50).
- **Safe boundary** — the last transcript entry is a *completed* assistant turn
  (not mid-tool-loop, no pending user message) AND the transcript has been
  quiet for `quiet_period_secs` (default 45s).

Then a **grace window** (default 10s) sends a "handing off in 10s — type to
defer" notice into the pane. Any keystroke or new transcript turn cancels and
resets. A per-session **cooldown** after a successful handoff prevents loops.

### 4. Handoff + swap (drive the live session)

At the safe point, for session `S` in pane `P`:

1. `tmux send-keys` into `P`: a prompt instructing the session to write a
   complete handoff document to a known path, then signal completion. (A custom
   prompt targeting a fixed file path — more reliable than relying on
   `/handoff`'s default output location, though it may reuse the handoff skill's
   structure.)
2. Daemon waits (bounded timeout) for the handoff file to appear/finish AND the
   transcript to show the turn completed.
3. Retire the old session cleanly (`/exit`).
4. `tmux respawn-pane -k -t P 'claude "Read the handoff at <path> and
   continue"'` — a fresh session with a tiny seed (low context, rot reduced),
   keeping the user in the same pane. The successor's `SessionStart` hook
   auto-registers it.
5. Append lineage record: old → new session, handoff path, timestamps.

The seed passes the handoff *path*, not its contents, to avoid huge/awkward
argv escaping; the new session reads the file itself.

## Configuration & state

- **Config:** `~/.config/context-manager/config.toml`
  - `threshold` (default 0.50)
  - `quiet_period_secs` (default 45)
  - `grace_secs` (default 10)
  - `cooldown_secs`
  - `model_windows` (map + fallback)
  - `dry_run` (bool)
  - optional per-cwd enable/disable
- **State:** `~/.local/share/context-manager/`
  - `sessions/` — registration files written by the hook
  - lineage log, daemon log

The hook owns only `sessions/`; the daemon owns everything else.

## Error handling

The daemon must never harm a user's session.

- Every tmux action is wrapped; on any failure → **abort the handoff, log, leave
  the session untouched**, and back off (no retry storms).
- If the handoff file never appears within timeout → abort and notify, never
  kill the session.
- **Dry-run mode** logs "would hand off" without acting, for safe live
  validation.

## Packaging

- `context-manager.service`, `systemctl --user`, `Restart=always`, logs to
  journald.
- **WSL2 caveat:** systemd `--user` requires systemd enabled in `wsl.conf`.
  Provide a `systemd-run` / tmux-resident supervisor fallback when `--user`
  systemd is unavailable.

## Testing

- **Unit (pure functions):** usage → context %, threshold logic, safe-point
  logic, model-window map resolution.
- **Integration:** a stub `claude` script running in a real tmux pane that
  appends to a fake JSONL, exercising the full detect → handoff → respawn swap.
- **Dry-run:** manual validation against real sessions before enabling action.

## Known risks / open items

- The **safe-point heuristic** relies on transcript quiescence; it cannot
  perfectly know the user is about to type. The grace window covers most cases.
  Accepted for v1.
- **Fully automatic terminal swapping is the riskiest behavior.** Build and
  validate in dry-run first before enabling live action.
- A small **race** remains if the user begins typing during key injection;
  mitigated by the idle + grace checks, accepted and documented for v1.
