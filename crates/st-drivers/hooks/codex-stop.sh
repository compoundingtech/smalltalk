#!/usr/bin/env bash
# st2 Codex Stop hook: expose messages that arrived since the previous idle checkpoint. Ding remains
# the live push path; this is the lifecycle backstop. Fail-open on every dependency/CLI error.

set -u

identity="${ST_AGENT:-}"
root="${ST_ROOT:-${CATALOG:-}}"
if [[ -z "$identity" || -z "$root" ]] || ! command -v st2 >/dev/null 2>&1 || ! command -v jq >/dev/null 2>&1; then
  exit 0
fi

state_base="${XDG_STATE_HOME:-${HOME-}/.local/state}"
safe_identity="$(printf '%s' "$identity" | tr -c 'A-Za-z0-9._-' '_')"
state_dir="${state_base}/st2/codex-hooks/${safe_identity}"
state_file="${state_dir}/last-checked.txt"
mkdir -p "$state_dir" 2>/dev/null || exit 0

last_checked=0
if [[ -f "$state_file" ]]; then
  raw="$(tr -d '[:space:]' < "$state_file" 2>/dev/null || true)"
  if [[ "$raw" =~ ^[0-9]+$ ]]; then
    last_checked="$raw"
  fi
fi

items_json="$(st2 message ls "$identity" --root "$root" --json --since "$last_checked" 2>/dev/null || printf '[]')"
if ! printf '%s' "$items_json" | jq -e 'type == "array"' >/dev/null 2>&1; then
  items_json='[]'
fi
# Advance to the greatest message timestamp actually observed. jq 1.6's `now` has only whole-second
# precision on macOS; using `now * 1000` would replay messages from the current second. An observed
# cursor also avoids skipping an arrival that lands between the list and cursor write.
next_checked="$(printf '%s' "$items_json" | jq --argjson previous "$last_checked" '
  ([.[].ts] | max) // $previous
')"
printf '%s\n' "$next_checked" > "$state_file" 2>/dev/null || true

count="$(printf '%s' "$items_json" | jq 'length')"
if (( count == 0 )); then
  exit 0
fi

additional="$(printf '%s' "$items_json" | jq -r --arg count "$count" '
  "New st2 messages arrived while this turn was running. Read, act on, reply when useful, and archive them before going idle.\n\n" +
  "## st2 inbox (" + $count + " new)\n" +
  (map(
    "- " + .filename
    + "  " + (.from // "unknown")
    + (if .subject != null then "  Subject: " + .subject else "" end)
  ) | join("\n"))
')"

jq -n --arg text "$additional" '{
  decision: "block",
  reason: $text
}'
exit 0
