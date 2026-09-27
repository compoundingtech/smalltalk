#!/usr/bin/env bash
set -euo pipefail

: "${ST_MISSION_RUN:?ST_MISSION_RUN must identify the judged mission run}"

messages="$("$ST3_BIN" message ls "agent/$ST_MISSION_RUN/sig.base" --archive --from "agent/$ST_MISSION_RUN/sig.sup" --json)"
matching="$(
  jq \
    --arg tag "mission-run:$ST_MISSION_RUN" \
    '[.[] | select(.tags | index($tag))]' \
    <<<"$messages"
)"

count="$(jq 'length' <<<"$matching")"
[ "$count" -eq 1 ] || {
  echo "FAIL: expected one final report for mission run $ST_MISSION_RUN, found $count"
  exit 1
}

body="$(jq -r '.[0].content' <<<"$matching")"
grep -qi 'beacon' <<<"$body"
grep -qiE 'commit|revision' <<<"$body"
grep -qiE 'test|green|pass' <<<"$body"

echo "PASS: the base owner received one tagged final report with revision and test evidence"
