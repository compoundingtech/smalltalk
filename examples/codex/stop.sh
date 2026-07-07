#!/usr/bin/env bash
# examples/codex/stop.sh — Codex Stop hook.
#
# Mirror of session-start.sh, run when the agent goes idle. Tracks a
# small state file with the last unix-ms checkpoint so each Stop only
# reports messages that arrived AFTER the previous Stop. Empty delta
# → silent exit; the agent goes idle uninterrupted.
#
# State path: $XDG_STATE_HOME/st-codex-hooks/last-checked.txt, or
# $HOME/.local/state/st-codex-hooks/last-checked.txt if XDG isn't
# set. The file holds a single decimal unix-ms integer.

set -u

emit_system_message() {
  if command -v jq >/dev/null 2>&1; then
    printf '%s' "$1" | jq -Rs '{systemMessage: ("st hook failed: " + .), continue: true}'
  else
    printf '{"systemMessage": "st hook failed: %s", "continue": true}\n' "$1"
  fi
}

# ─── Env + dep checks ─────────────────────────────────────────────────

if [ -z "${ST_ROOT:-}" ]; then
  printf 'st-codex-hook: ST_ROOT not set\n' >&2
  exit 1
fi

if [ -z "${ST_AGENT:-}" ]; then
  printf 'st-codex-hook: ST_AGENT not set\n' >&2
  exit 1
fi

if ! command -v st >/dev/null 2>&1; then
  printf 'st-codex-hook: st not on PATH\n' >&2
  exit 1
fi

if ! command -v jq >/dev/null 2>&1; then
  printf 'st-codex-hook: jq not on PATH (required by this hook)\n' >&2
  exit 1
fi

# ─── State file ───────────────────────────────────────────────────────

state_dir="${XDG_STATE_HOME:-$HOME/.local/state}/st-codex-hooks"
state_file="$state_dir/last-checked.txt"

if ! mkdir -p "$state_dir" 2>/dev/null; then
  emit_system_message "could not create state dir: $state_dir"
  exit 0
fi

last_checked=0
if [ -f "$state_file" ]; then
  raw=$(cat "$state_file" 2>/dev/null)
  if [[ "$raw" =~ ^[0-9]+$ ]]; then
    last_checked="$raw"
  fi
fi

# Generate a millisecond timestamp portably (no `date +%N` on macOS).
now_ms=$(jq -n 'now * 1000 | floor')

# ─── Read inbox delta ─────────────────────────────────────────────────

# st message ls --since filters by filename's <unix-ms> prefix. Files that
# arrived via sync with an older prefix are missed by design — Stop is
# a notification cue, not a backfill audit.
#
# brief-005-phase0: capture stdout and stderr separately so the
# `[smalltalk] honoring ST_AGENT` warning doesn't corrupt the
# JSON payload while still being available for the failure-diagnostic
# path below.
err_file=$(mktemp -t st-hook-err.XXXXXX)
trap "rm -f '$err_file'" EXIT
if ! items_json=$(st message ls --json --since "$last_checked" 2>"$err_file"); then
  emit_system_message "st message ls --json failed: $(cat "$err_file")"
  exit 0
fi

count=$(printf '%s' "$items_json" | jq 'length')

# Always advance the cursor so the next Stop sees a fresh window.
printf '%s' "$now_ms" > "$state_file"

if [ "$count" -eq 0 ]; then
  exit 0
fi

# ─── Build payload ────────────────────────────────────────────────────

header="## st inbox ($count new since last check)"

printf '%s' "$items_json" | jq \
  --arg header "$header" \
  '{
    additionalContext: (
      $header + "\n" +
      (map(
        "- " + .filename
        + "  " + (.from // "unknown")
        + (if .subject != null then "  Subject: " + .subject else "" end)
      ) | join("\n"))
    ),
    continue: true
  }'
