#!/usr/bin/env bash
# examples/codex/session-start.sh — Codex SessionStart hook.
#
# Reads $ST_ROOT/$ST_AGENT/inbox/ via `st message ls --json` and
# emits a Codex hook payload that injects the unread snapshot as
# additionalContext at the start of the session. Empty inbox → silent
# exit (no payload). Missing env or `st` / `jq` not on PATH →
# non-zero exit with a stderr message; the hook is configured in
# Codex with timeout/continue semantics so the session keeps going.
# In-band failures (e.g. `st message ls` returns non-zero for a permission
# error) → emit `{systemMessage,continue:true}` so Codex shows the
# reason and continues.
#
# Drop this into ~/.codex/hooks/ and reference it from
# ~/.codex/config.toml — see config.toml.example next to this file.

set -u

emit_system_message() {
  # JSON-escape the reason via jq if available; otherwise hope it's
  # safe (the only callers below pass static strings).
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

# ─── Read inbox ───────────────────────────────────────────────────────

# brief-005-phase0: capture stdout and stderr separately so warnings
# (e.g. "[smalltalk] honoring ST_AGENT") don't corrupt the JSON
# payload. The stderr stream is preserved for the failure-diagnostic
# path below.
err_file=$(mktemp -t st-hook-err.XXXXXX)
trap "rm -f '$err_file'" EXIT
if ! items_json=$(st message ls --json 2>"$err_file"); then
  emit_system_message "st message ls --json failed: $(cat "$err_file")"
  exit 0
fi

count=$(printf '%s' "$items_json" | jq 'length')
if [ "$count" -eq 0 ]; then
  # Empty inbox: emit nothing, let Codex start without an injected block.
  exit 0
fi

# ─── Build payload ────────────────────────────────────────────────────

# additionalContext is a single string with newlines escaped — let jq
# build it so the final envelope is valid JSON regardless of subject /
# from field contents.
header="## st inbox ($count unread)"

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
