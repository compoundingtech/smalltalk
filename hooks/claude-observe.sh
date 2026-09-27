#!/usr/bin/env bash
# st2 Claude observe hook: forward one hook event (name in $1, payload on stdin) to the agent's
# observed-harness-state record. Ordinary observation fails open. A residency SessionStart
# propagates failure because its native-session binding is launch authority, not telemetry.

set -u

event="${1:-}"
identity="${ST2_CLAUDE_IDENTITY:-${ST_AGENT:-}}"
# CATALOG-first, deliberately diverging from the sibling hooks' ST_ROOT-first order: their
# ST_ROOT is a bus root for message writes, while --catalog here resolves the agent DECLARATION —
# with a custom bus root (ST_ROOT != CATALOG) declaration resolution under ST_ROOT finds nothing
# and every transition would silently drop.
root="${CATALOG:-${ST_ROOT:-}}"
mandatory_binding=""
if [[ "$event" == "SessionStart" ]]; then
  mandatory_binding="${ST2_CLAUDE_RESUME_GENERATION:-}${ST2_CLAUDE_EXPECTED_NATIVE_SESSION:-}"
fi
runtime_id="${ST2_CLAUDE_RUNTIME_ID:-$identity}"
if [[ -z "$event" || -z "$identity" || -z "$root" ]] || ! command -v st2 >/dev/null 2>&1; then
  [[ -n "$mandatory_binding" ]] && exit 1
  exit 0
fi

if [[ -n "$mandatory_binding" ]]; then
  exec st2 --catalog "$root" driver claude-observe --identity "$identity" --runtime-id "$runtime_id" \
    --event "$event" >/dev/null 2>&1
fi

st2 --catalog "$root" driver claude-observe --identity "$identity" --runtime-id "$runtime_id" \
  --event "$event" >/dev/null 2>&1 || true
exit 0
