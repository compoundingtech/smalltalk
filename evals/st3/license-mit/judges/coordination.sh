#!/usr/bin/env bash
set -euo pipefail

: "${ST_MISSION_RUN:?ST_MISSION_RUN must identify the judged mission run}"
supervisor="agent/$ST_MISSION_RUN/lmc.sup"
worker="agent/$ST_MISSION_RUN/lmc.worker"

messages_to() {
  local recipient=$1
  jq -s 'add | unique_by(.subject)' \
    <(st3 conversations ls "$recipient" --json) \
    <(st3 conversations ls "$recipient" --archive --json)
}

worker_messages="$(messages_to "$worker")"
supervisor_messages="$(messages_to "$supervisor")"
requester_messages="$(messages_to person/eval-requester)"

delegation_index="$(jq --arg sender "$supervisor" \
  '[.[] | select(.from == $sender) | .created_index] | min // 0' \
  <<<"$worker_messages")"
report_index="$(jq --arg sender "$worker" \
  '[.[] | select(.from == $sender) | .created_index] | min // 0' \
  <<<"$supervisor_messages")"
confirmation_index="$(jq --arg sender "$supervisor" \
  '[.[] | select(.from == $sender) | .created_index] | max // 0' \
  <<<"$requester_messages")"

test "$delegation_index" -gt 0
test "$report_index" -gt "$delegation_index"
test "$confirmation_index" -gt "$report_index"

echo "PASS: delegation, worker report, and later confirmation are visible"
