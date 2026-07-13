#!/usr/bin/env bash
# examples/codex/session-start.sh — Codex SessionStart hook.
#
# Emits a Codex hook payload that injects, as additionalContext at the
# start of the session:
#   1. the agent's last durable working-state ($ST_ROOT/$ST_AGENT/
#      context/now.md), staleness-guarded — parity with the claude
#      session-start hook so codex agents are context-restorable on
#      cold-boot too; and
#   2. the unread inbox snapshot via `st message ls --json`.
# Both absent (no fresh now.md AND empty inbox) → silent exit (no
# payload). Missing env or `st` / `jq` not on PATH →
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

# ─── Restore working-state: context/now.md (parity with claude hook) ───
#
# brief-024 hook-legs parity: inject the agent's last durable
# working-state so a cold-started codex agent picks the task back up
# instead of reconstructing from scratch. Absent-able + staleness-
# guarded exactly like examples/claude-code/hooks/session-start.sh:
# now.md missing, or aged past $ST_REHYDRATE_STALE_S (default 24h), is
# skipped — stale context is worse than none — leaving the pre-parity
# behavior (inbox-only, or silent) intact.
stale_s="${ST_REHYDRATE_STALE_S:-86400}"
now_md="$ST_ROOT/$ST_AGENT/context/now.md"
now_block=""
if [ -f "$now_md" ]; then
  # BSD (-f %m) + GNU (-c %Y) stat fallback; 0 on failure reads as stale.
  now_mtime="$(stat -f %m "$now_md" 2>/dev/null || stat -c %Y "$now_md" 2>/dev/null || echo 0)"
  if [[ "$now_mtime" =~ ^[0-9]+$ ]] && (( now_mtime > 0 )); then
    age_s=$(( $(date +%s) - now_mtime ))
    if (( age_s >= 0 && age_s < stale_s )); then
      # Same <context source=...> envelope the claude hook emits, so
      # downstream consumers (evals, log analyzers) recognize it. The
      # trailing-newline dance keeps </context> on its own line whether
      # or not now.md ends in a newline.
      now_block="$(
        printf '<context source="st/context/now.md" agent="%s">\n' "$ST_AGENT"
        cat "$now_md"
        [ -n "$(tail -c 1 "$now_md" 2>/dev/null)" ] && printf '\n'
        printf '</context>'
      )"
    fi
  fi
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
inbox_text=""
if [ "$count" -gt 0 ]; then
  # Render the unread snapshot as a plain string (jq -r) so it composes
  # with the now.md block before the final JSON envelope is built.
  header="## st inbox ($count unread)"
  inbox_text="$(printf '%s' "$items_json" | jq -r \
    --arg header "$header" \
    '$header + "\n" + (map(
      "- " + .filename
      + "  " + (.from // "unknown")
      + (if .subject != null then "  Subject: " + .subject else "" end)
    ) | join("\n"))')"
fi

# ─── Combine + emit ───────────────────────────────────────────────────
#
# additionalContext = now.md block (if fresh) then the inbox snapshot
# (if any), blank-line-separated. If BOTH are absent — no fresh state
# AND empty inbox — emit nothing and let codex start clean (unchanged
# behavior for the no-now.md + empty-inbox case).
if [ -n "$now_block" ] && [ -n "$inbox_text" ]; then
  additional="${now_block}"$'\n\n'"${inbox_text}"
elif [ -n "$now_block" ]; then
  additional="$now_block"
elif [ -n "$inbox_text" ]; then
  additional="$inbox_text"
else
  exit 0
fi

# jq -Rs turns the raw combined string into a single JSON string,
# escaping newlines/quotes so the envelope is valid regardless of
# now.md / subject / from contents.
printf '%s' "$additional" | jq -Rs '{additionalContext: ., continue: true}'
