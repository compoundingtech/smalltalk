#!/usr/bin/env bash
# st2 Codex PreCompact hook. Never block compaction. If the agent has not captured any durable
# working state, write a reconstruction stub; never overwrite non-whitespace state.

set -u

identity="${ST_AGENT:-}"
root="${ST_ROOT:-${CATALOG:-}}"
if [[ -z "$identity" || -z "$root" ]] || ! command -v st2 >/dev/null 2>&1; then
  exit 0
fi

existing="$(st2 context read "$identity" --root "$root" 2>/dev/null || true)"
if [[ -n "$(printf '%s' "$existing" | tr -d '[:space:]')" ]]; then
  exit 0
fi

timestamp="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
stub="# now — pre-compact stub — ${timestamp}

PreCompact fired before the model captured durable working state. Reconstruct from git status,
recent commits, and the st2 inbox, then write a real checkpoint with \`st2 context write\`."

timeout_s="${ST_PRECOMPACT_TIMEOUT_S:-5}"
if command -v timeout >/dev/null 2>&1; then
  printf '%s\n' "$stub" | timeout "${timeout_s}s" st2 context write "$identity" --root "$root" >/dev/null 2>&1 || true
elif command -v gtimeout >/dev/null 2>&1; then
  printf '%s\n' "$stub" | gtimeout "${timeout_s}s" st2 context write "$identity" --root "$root" >/dev/null 2>&1 || true
else
  printf '%s\n' "$stub" | st2 context write "$identity" --root "$root" >/dev/null 2>&1 || true
fi

exit 0
