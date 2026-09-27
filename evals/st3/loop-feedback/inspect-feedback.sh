#!/bin/sh
set -eu

if [ "$ST_LOOP_ROUND" -eq 1 ]; then
  test -z "$ST_LOOP_FEEDBACK"
  exit 0
fi

test -n "$ST_LOOP_FEEDBACK"
"$ST3_BIN" --endpoint "$ST3_ENDPOINT" doc get "$ST_LOOP_FEEDBACK" --output feedback.json
grep -F '"round": 1' feedback.json >/dev/null
grep -F '"mission_run": "mission-run/' feedback.json >/dev/null
