#!/usr/bin/env bash
set -Eeuo pipefail

readonly EVAL_ROOT=$PWD
readonly REQUESTER=person/eval-requester
readonly REQUEST="$EVAL_ROOT/request.md"

trace_pid=""

stop_trace() {
  if [[ -n "$trace_pid" ]] && kill -0 "$trace_pid" 2>/dev/null; then
    kill -TERM "$trace_pid" 2>/dev/null || true
    wait "$trace_pid" 2>/dev/null || true
  fi
}

cleanup() {
  stop_trace
}

trap cleanup EXIT HUP INT TERM

wait_for_event() {
  local subject=$1
  local kind=$2
  local output=$3
  local deadline=$((SECONDS + 1200))

  : > "$output"
  st3 --json trace "$subject" --after-index 0 --follow > "$output" &
  trace_pid=$!
  while (( SECONDS < deadline )); do
    if jq -e --arg kind "$kind" 'select(.kind == $kind)' "$output" >/dev/null 2>&1; then
      stop_trace
      trace_pid=""
      return 0
    fi
    if ! kill -0 "$trace_pid" 2>/dev/null; then
      wait "$trace_pid"
      return 1
    fi
    sleep 0.2
  done
  printf 'Timed out waiting for %s on %s.\n' "$kind" "$subject" >&2
  return 1
}

st3 publish "$EVAL_ROOT/initial.kdl" --as "$REQUESTER" >/dev/null
st3 --json mission start generation-proof \
  --id "generation-proof/${ST_MISSION_RUN}" \
  --workspace "$EVAL_ROOT" \
  --as "$REQUESTER" > "$EVAL_ROOT/initial-run.json"

run_subject=$(jq -er '.mission_run.subject' "$EVAL_ROOT/initial-run.json")
run_id=$(jq -er '.mission_run.id' "$EVAL_ROOT/initial-run.json")
old_generation=$(jq -er '.mission_run.generation' "$EVAL_ROOT/initial-run.json")
stable_subject=$(jq -er '.mission_run.steps[] | select(.step == "stable") | .subject' "$EVAL_ROOT/initial-run.json")

st3 claim "resource/generation-proof/$run_id/stable" resource.observed \
  --actor "$REQUESTER" \
  --field kind=custom.st3.eval-signal \
  --field state=done >/dev/null
st3 trace wait "$stable_subject" --for completed --timeout 2m >/dev/null

st3 --json launch start \
  --run "$run_subject" \
  "$REQUEST" \
  --workspace "$EVAL_ROOT" \
  --as "$REQUESTER" \
  --model gpt-5.6-sol \
  --effort medium \
  > "$EVAL_ROOT/planning-started.json"

session_id=$(jq -er '.id' "$EVAL_ROOT/planning-started.json")
session_subject=$(jq -er '.subject' "$EVAL_ROOT/planning-started.json")
planner=$(jq -er '.planner' "$EVAL_ROOT/planning-started.json")
wait_for_event \
  "$session_subject" \
  "planning-session.candidate-submitted" \
  "$EVAL_ROOT/planning-events.jsonl"

st3 --json work revision generation "$old_generation" > "$EVAL_ROOT/before-approval.json"
jq -e --arg revision "$(jq -er '.mission_run.revision' "$EVAL_ROOT/initial-run.json")" '
  .status == "running"
  and .revision == $revision
' "$EVAL_ROOT/before-approval.json" >/dev/null

variant=$(st3 --json launch show "$session_id" | jq -er '.candidate.variant')
st3 --json launch preview "$session_id" --variant "$variant" > "$EVAL_ROOT/preview.json"
preview_hash=$(jq -er '.preview.hash' "$EVAL_ROOT/preview.json")
jq -e '
  .status == "review"
  and (.preview.graph | contains("stable [root]"))
  and (.preview.graph | contains("changed [after stable]"))
  and (.preview.graph | contains("generation-environment [root]"))
  and (.preview.diff | contains("update mission/generation-proof"))
  and (.preview.mission.blockers | length == 0)
' "$EVAL_ROOT/preview.json" >/dev/null

st3 --json launch approve \
  "$session_id" \
  "$preview_hash" \
  --as "$REQUESTER" \
  > "$EVAL_ROOT/approved.json"

jq -e --arg hash "$preview_hash" '
  .status == "approved"
  and .preview.hash == $hash
  and .published_revision == .candidate.mission_revision
  and .target_mission_run != null
  and .source_generation != null
' "$EVAL_ROOT/approved.json" >/dev/null

st3 --json mission show "$run_subject" > "$EVAL_ROOT/applied.json"
new_generation=$(jq -er '.generation' "$EVAL_ROOT/applied.json")
environment_subject=$(jq -er '.steps[] | select(.step == "generation-environment") | .subject' "$EVAL_ROOT/applied.json")
st3 trace wait "$environment_subject" --for completed --timeout 2m >/dev/null

st3 --json work revision generations "$run_subject" > "$EVAL_ROOT/generations.json"
st3 --json work revision generation "$old_generation" > "$EVAL_ROOT/old-generation.json"
st3 --json work revision generation "$new_generation" > "$EVAL_ROOT/new-generation.json"

jq -e --arg old "$old_generation" --arg new "$new_generation" '
  .generation == $new
  and .generation != $old
  and .initial_revision != .revision
' "$EVAL_ROOT/applied.json" >/dev/null

jq -e --arg old "$old_generation" --arg new "$new_generation" '
  length == 2
  and .[0].subject == $old
  and .[0].status == "superseded"
  and .[1].subject == $new
  and .[1].predecessor == $old
  and .[1].status == "running"
' "$EVAL_ROOT/generations.json" >/dev/null

jq -e '
  .status == "superseded"
  and (.steps[] | select(.step == "stable") | .status) == "completed"
' "$EVAL_ROOT/old-generation.json" >/dev/null
jq -e '
  (.steps[] | select(.step == "stable") | .status) == "completed"
  and (.steps[] | select(.step == "changed") | .status) != "completed"
' "$EVAL_ROOT/new-generation.json" >/dev/null

test "$(cat "$EVAL_ROOT/observed-generation.txt")" = "${new_generation#run-generation/}"

st3 claim "resource/generation-proof/$run_id/changed" resource.observed \
  --actor "$REQUESTER" \
  --field kind=custom.st3.eval-signal \
  --field state=done >/dev/null
st3 trace wait "$run_subject" --for completed --timeout 2m >/dev/null
st3 trace wait "$planner" --for stopped --timeout 2m >/dev/null

printf '%s\n' RUN-GENERATION-REVISION-GREEN > "$EVAL_ROOT/result.txt"
