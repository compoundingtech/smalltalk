#!/usr/bin/env bash
set -Eeuo pipefail
exec >controller.log 2>&1

readonly PLANNER="agent/${ST_MISSION_RUN}/planner"
readonly PRODUCE="step-run/${ST_RUN_GENERATION}/produce"
produced_run=""

cleanup() {
  if [[ -n "$produced_run" ]]; then
    printf 'version 2\nmission-run "%s" { cancellation "eval-finished" { reason "the authority eval finished" } }\n' "$produced_run" \
      | st3 publish - --as person/eval-requester >/dev/null 2>&1 || true
  fi
}

trap cleanup EXIT HUP INT TERM

st3 trace wait "$PLANNER" --for running --timeout 1m >/dev/null
st3 work claim "$PRODUCE" --as "$PLANNER" >/dev/null
st3 work publish-mission "$PRODUCE" produced.kdl --as "$PLANNER" > published.txt
grep -Fq 'mission/eval/mission-authority/produced@' published.txt
st3 work complete "$PRODUCE" --as "$PLANNER" --summary "the mission was published" >/dev/null

if st3 publish produced.kdl --as "$PLANNER" > direct.out 2> direct.err; then
  printf '%s\n' 'generic agent mission publication unexpectedly succeeded' >&2
  exit 1
fi
grep -Fq 'agent-mission-publication-route' direct.err

st3 --json mission start eval/mission-authority/produced \
  --id "eval/mission-authority/produced/${ST_MISSION_RUN}" \
  --workspace "$PWD" \
  --as "$PLANNER" > started.json
produced_run=$(jq -er '.mission_run.subject' started.json)
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

if st3 missions start eval/mission-authority/produced \
  --id "eval/mission-authority/denied/${ST_MISSION_RUN}" \
  --workspace "$PWD" \
  --as "$REVISER" > denied.out 2> denied.err; then
  printf '%s\n' 'an agent without start authority unexpectedly started a mission' >&2
  exit 1
fi
grep -Fq 'mission-authority-denied' denied.err

printf '%s\n' MISSION-AUTHORITY-GREEN > result.txt
