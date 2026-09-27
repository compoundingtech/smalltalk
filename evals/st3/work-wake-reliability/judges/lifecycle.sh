#!/usr/bin/env bash
set -euo pipefail

: "${ST_MISSION_RUN:?ST_MISSION_RUN must identify the judged mission run}"
test -s controller-state.json

jq -e '
  (.incarnations | length) == 2
  and .incarnations[0] != .incarnations[1]
  and ([.scenarios[] | select(.kind == "fresh" and .status == "completed")] | length) == 3
  and ([.scenarios[] | select(
    .kind == "revision"
    and .revisions == 2
    and .status == "cancelled"
    and .finalizer == "completed"
  )] | length) == 1
  and (.steps | length) == 6
  and ([.steps[].scenario] | sort) == ([
    "fresh-after-restart",
    "fresh-one",
    "fresh-two",
    "revision-initial",
    "revision-one",
    "revision-two"
  ] | sort)
  and ([.steps[].wake.attempts] | all(. >= 1))
  and ([.steps[].wake.failure] | all(. == null))
  and ([.steps[].wake.acknowledged_by] | all(. != null))
' controller-state.json >/dev/null

trace_file="$(mktemp)"
trap 'rm -f "$trace_file"' EXIT
st3 trace show --json --limit 500 >"$trace_file"

while IFS= read -r subject; do
  work_snapshot="$(st3 work show "$subject" --json)"
  jq -e '.value.state == "completed"' <<<"$work_snapshot" >/dev/null

  wake_prefix="st3-work:$subject@"
  wake_message="$(jq -sr --arg prefix "$wake_prefix" --arg worker "$(jq -r .worker controller-state.json)" '
    [ .[] | select(
      .kind == "message.sent"
      and .body.fields.to == $worker
      and any(.body.fields.tags[]?; startswith($prefix))
    ) ]
    | select(length == 1)
    | .[0].subject
  ' "$trace_file")"
  test -n "$wake_message"

  wake_sent_at="$(jq -sr --arg message "$wake_message" '
    .[] | select(.subject == $message and .kind == "message.sent") | .accepted_at_unix_ms
  ' "$trace_file")"
  claim_at="$(st3 trace show "$subject" --json --limit 100 \
    | jq -sr '[.[] | select(.kind == "work.claimed")] | select(length == 1) | .[0].accepted_at_unix_ms')"
  test -n "$wake_sent_at"
  test -n "$claim_at"
  test "$wake_sent_at" -le "$claim_at"
done < <(jq -r '.steps[].subject' controller-state.json)

echo "PASS: six assigned steps woke and completed across fresh runs, revisions, and worker replacement; cancellation finalized"
