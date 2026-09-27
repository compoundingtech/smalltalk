#!/usr/bin/env bash
set -euo pipefail

: "${ST_MISSION_RUN:?ST_MISSION_RUN must identify the judged mission run}"
requester_inbox="$(st3 conversations ls person/eval-requester --json)"
requester_archive="$(st3 conversations ls person/eval-requester --archive --json)"
requester_messages="$(jq -s 'add | unique_by(.subject)' \
  <(printf '%s' "$requester_inbox") \
  <(printf '%s' "$requester_archive"))"

jq -e \
  --arg sender "agent/$ST_MISSION_RUN/fd.sup" \
  --arg run_tag "mission-run:$ST_MISSION_RUN" \
  '[.[] | select(.from == $sender and (.tags | index($run_tag)))] | length == 1' \
  <<<"$requester_messages" >/dev/null

echo "PASS: the supervisor sent one recommendation to the requester"
