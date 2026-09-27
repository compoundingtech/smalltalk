#!/usr/bin/env bash
set -Eeuo pipefail
exec >controller.log 2>&1

readonly REQUESTER=person/eval-requester
run_subject=""

cleanup() {
  if [[ -n "$run_subject" ]]; then
    printf 'version 2\nmission-run "%s" { cancellation "eval-finished" { reason "the queue eval finished" } }\n' "$run_subject" \
      | st3 publish - --as "$REQUESTER" >/dev/null 2>&1 || true
  fi
}

trap cleanup EXIT HUP INT TERM

st3 --json mission start eval/queue-revision/work \
  --id "eval/queue-revision/work/${ST_MISSION_RUN}" \
  --workspace "$PWD" \
  --as "$REQUESTER" > initial-run.json

run_subject=$(jq -er '.mission_run.subject' initial-run.json)
old_generation=$(jq -er '.mission_run.generation' initial-run.json)
st3 trace wait "$run_subject" --for standing --timeout 1m >/dev/null
st3 --json mission show "$run_subject" > initial-standing.json

jq -e '
  [.steps | sort_by(.queue_position)[] | {step, queue, queue_position, status}]
  == [
    {step:"one", queue:"investigations", queue_position:1, status:"completed"},
    {step:"two", queue:"investigations", queue_position:2, status:"completed"},
    {step:"three", queue:"investigations", queue_position:3, status:"completed"}
  ]
' initial-standing.json >/dev/null

st3 --json work revise "$run_subject" revised.kdl \
  --as "$REQUESTER" \
  --reason "put the third investigation before the second" > revised.json

jq -e '
  .status == "applied"
  and ([.mission_run.steps | sort_by(.queue_position)[] | {step, queue, queue_position, status}]
    == [
      {step:"one", queue:"investigations", queue_position:1, status:"completed"},
      {step:"three", queue:"investigations", queue_position:2, status:"pending"},
      {step:"two", queue:"investigations", queue_position:3, status:"pending"}
    ])
' revised.json >/dev/null

new_generation=$(jq -er '.mission_run.generation' revised.json)
[[ "$new_generation" != "$old_generation" ]]
st3 --json work revision generations "$run_subject" > generations.json
jq -e --arg old "$old_generation" --arg new "$new_generation" '
  length == 2
  and .[0].subject == $old
  and .[0].status == "superseded"
  and .[1].subject == $new
  and .[1].predecessor == $old
' generations.json >/dev/null

printf '%s\n' QUEUE-REVISION-GREEN > result.txt
