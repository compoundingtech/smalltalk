#!/usr/bin/env bash
set -euo pipefail

: "${ST_MISSION_RUN:?ST_MISSION_RUN must identify the judged mission run}"

if [ -s pty-audit.log ]; then
  printf 'direct terminal interaction was observed:\n' >&2
  sed -n '1,40p' pty-audit.log >&2
  exit 1
fi

worker="agent/$ST_MISSION_RUN/wake.worker"
count="$(st3 trace show "$worker" --json --limit 500 \
  | jq -s '[.[] | select(.kind == "terminal.input.requested")] | length')"
test "$count" -eq 0

echo "PASS: no graph or executable terminal-input path participated"
