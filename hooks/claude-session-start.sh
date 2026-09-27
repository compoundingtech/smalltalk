#!/usr/bin/env bash
# st2 Claude SessionStart hook: restore fresh durable context and remind the model to complete its
# boot ritual. Delivered as `hookSpecificOutput.additionalContext` on stdout, the supported channel —
# exit 2 + stderr renders as a hook error and never reaches the model. Missing state remains a valid
# cold start. Fail-open: a missing dependency never prevents Claude startup.

set -u

identity="${ST2_CLAUDE_IDENTITY:-${ST_AGENT:-}}"
root="${ST_ROOT:-${CATALOG:-}}"
ritual="Run the st2 boot ritual now: set your status to available, then drain your inbox by reading, acting on, replying when useful, and archiving each handled message. Before resuming or starting work, set your status to busy; set available only when yielding or ready for new work."

if ! command -v jq >/dev/null 2>&1; then
  exit 0
fi

context=""
if [[ -n "$identity" && -n "$root" ]] && command -v st2 >/dev/null 2>&1; then
  stale_s="${ST_REHYDRATE_STALE_S:-86400}"
  context="$(st2 context read "$identity" --root "$root" --fresh-within "$stale_s" 2>/dev/null || true)"
fi

context_block=""
if [[ -n "$(printf '%s' "$context" | tr -d '[:space:]')" ]]; then
  context_block="$(
    printf '<context source="st2/context/now.md" agent="%s">\n' "$identity"
    printf '%s' "$context"
    [[ "$context" == *$'\n' ]] || printf '\n'
    printf '</context>'
  )"
fi

additional="$ritual"
if [[ -n "$context_block" ]]; then
  additional="${context_block}"$'\n\n'"${additional}"
fi

printf '%s' "$additional" | jq -Rs '{
  continue: true,
  hookSpecificOutput: {
    hookEventName: "SessionStart",
    additionalContext: .
  }
}'
exit 0
