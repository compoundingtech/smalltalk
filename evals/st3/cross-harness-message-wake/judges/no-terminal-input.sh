#!/usr/bin/env bash
set -euo pipefail

: "${ST_MISSION_RUN:?ST_MISSION_RUN must identify the judged mission run}"

if [ -s pty-audit.log ]; then
  printf 'direct terminal interaction was observed:\n' >&2
  sed -n '1,40p' pty-audit.log >&2
  exit 1
fi

for name in codex claude pi omp; do
  agent="agent/$ST_MISSION_RUN/wake.$name"
  count="$(st3 trace show "$agent" --json --limit 200 \
    | jq -s '[.[] | select(.kind == "terminal.input.requested")] | length')"
  test "$count" -eq 0
done

echo "PASS: no harness used the graph terminal-input API or a direct terminal executable"
