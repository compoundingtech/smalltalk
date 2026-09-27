#!/usr/bin/env bash
set -euo pipefail

: "${ST_MISSION_RUN:?ST_MISSION_RUN must identify the judged mission run}"
source ./judges/history.sh

if [ -s pty-audit.log ]; then
  printf 'direct terminal interaction was observed:\n' >&2
  sed -n '1,40p' pty-audit.log >&2
  exit 1
fi

history="$(mktemp)"
trap 'rm -f "$history"' EXIT
dump_history "$history"

count="$(jq -s '[.[] | select(.kind == "terminal.input.requested")] | length' "$history")"
test "$count" -eq 0

echo "PASS: no graph or executable terminal-input path participated"
