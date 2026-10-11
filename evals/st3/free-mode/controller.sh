#!/usr/bin/env bash
set -Eeuo pipefail
exec >controller.log 2>&1

readonly PLANNER="agent/${ST_MISSION_RUN}/planner"
readonly PRODUCE="step-run/${ST_RUN_GENERATION}/produce"
runs=()

cleanup() {
  for run in "${runs[@]}"; do
    printf 'version 2\nmission-run "%s" { cancellation "eval-finished" { reason "the free mode eval finished" } }\n' "$run" \
      | st3 publish - --as person/eval-requester >/dev/null 2>&1 || true
  done
}

trap cleanup EXIT HUP INT TERM

st3 trace wait "$PLANNER" --for running --timeout 1m >/dev/null
st3 work claim "$PRODUCE" --as "$PLANNER" >/dev/null
st3 work publish-mission "$PRODUCE" produced.kdl --as "$PLANNER" > published.txt
grep -Fq 'mission/eval/free-mode/produced@' published.txt
st3 work complete "$PRODUCE" --as "$PLANNER" --summary "the mission was published" >/dev/null

# Free mode: generic publication by an ungranted agent succeeds too.
st3 apply produced.kdl --as "$PLANNER" > direct.out

st3 --json missions start eval/free-mode/produced \
  --id "eval/free-mode/produced/${ST_MISSION_RUN}" \
  --workspace "$PWD" \
  --as "$PLANNER" > started.json
produced_run=$(jq -er '.mission_run.subject' started.json)
runs+=("$produced_run")
jq -e --arg planner "$PLANNER" '.mission_run.requester == $planner' started.json >/dev/null
produced_run_id=${produced_run#mission-run/}
readonly REVISER="agent/${produced_run_id}/reviser"
st3 trace wait "$REVISER" --for running --timeout 1m >/dev/null
st3 trace wait "$produced_run" --for standing --timeout 1m >/dev/null

st3 --json work revise "$produced_run" revised.kdl \
  --as "$REVISER" \
  --reason "clarify the standing mission goal" > revised.json
jq -e '
  .status == "applied"
  and .mission_run.initial_revision != .mission_run.revision
' revised.json >/dev/null
st3 trace wait "$produced_run" --for standing --timeout 1m >/dev/null

st3 --json missions start eval/free-mode/produced \
  --id "eval/free-mode/second/${ST_MISSION_RUN}" \
  --workspace "$PWD" \
  --as "$REVISER" > second.json
runs+=("$(jq -er '.mission_run.subject' second.json)")
jq -e --arg reviser "$REVISER" '.mission_run.requester == $reviser' second.json >/dev/null

printf '%s\n' FREE-MODE-GREEN > result.txt
