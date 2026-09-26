#!/usr/bin/env bash
set -Eeuo pipefail

: "${ST_MISSION_RUN:?ST_MISSION_RUN must identify the eval mission run}"

readonly REQUESTER=person/eval-requester
readonly WORKER="agent/$ST_MISSION_RUN/wake.worker"
readonly GENERATED="$PWD/generated"
readonly STATE="$PWD/controller-state.json"

revisable_run=""

cleanup() {
  if [[ -n "$revisable_run" ]]; then
    st3 missions cancel "$revisable_run" \
      --reason "the reliability controller is cleaning up after an early exit" \
      --as "$REQUESTER" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT HUP INT TERM

mkdir -p "$GENERATED"

state="$(jq -n --arg worker "$WORKER" '{worker: $worker, scenarios: [], steps: [], incarnations: []}')"
persist_state() {
  printf '%s\n' "$state" >"$STATE"
}
persist_state

agent_json() {
  st3 agents show "$WORKER" --all --json 2>/dev/null
}

wait_for_worker() {
  local expected_incarnation=${1:-} deadline=$((SECONDS + 300)) snapshot incarnation
  while (( SECONDS < deadline )); do
    snapshot="$(agent_json || true)"
    incarnation="$(jq -r '.value.incarnation_id // empty' <<<"${snapshot:-{}}" 2>/dev/null || true)"
    if jq -e '
      .value.state == "running"
      and .value.reachability == "reachable"
      and .value.harness_state == "idle"
    ' <<<"$snapshot" >/dev/null 2>&1 \
      && [[ -n "$incarnation" ]] \
      && { [[ -z "$expected_incarnation" ]] || [[ "$incarnation" != "$expected_incarnation" ]]; }; then
      printf '%s\n' "$incarnation"
      return 0
    fi
    sleep 0.5
  done
  printf 'worker did not reach exact idle in a suitable incarnation\n' >&2
  return 1
}

render_fixture() {
  local source=$1 destination=$2
  sed "s|{{WORKER}}|$WORKER|g" "$source" >"$destination"
}

record_completed_step() {
  local scenario=$1 subject=$2 work_snapshot run snapshot
  st3 trace wait "$subject" --for completed --timeout 5m >/dev/null
  work_snapshot="$(st3 work show "$subject" --json)"
  run="$(jq -er '.value.mission_run_id' <<<"$work_snapshot")"
  snapshot="$(st3 missions show "$run" --json \
    | jq -e --arg subject "$subject" '.steps[] | select(.subject == $subject)')"
  jq -e '
    .status == "completed"
    and .wake.attempts >= 1
    and .wake.failure == null
    and .wake.acknowledged_by != null
  ' <<<"$snapshot" >/dev/null
  state="$(jq \
    --arg scenario "$scenario" \
    --arg subject "$subject" \
    --argjson wake "$(jq '.wake' <<<"$snapshot")" \
    '.steps += [{scenario: $scenario, subject: $subject, wake: $wake}]' <<<"$state")"
  persist_state
  wait_for_worker >/dev/null
}

start_fresh() {
  local label=$1 output run step
  output="$(st3 missions start eval/work-wake-reliability/fresh \
    --id "fresh-$label-$ST_MISSION_RUN" \
    --workspace "$PWD" \
    --as "$REQUESTER" \
    --json)"
  run="$(jq -er '.mission_run.subject' <<<"$output")"
  step="$(jq -er '.mission_run.steps[] | select(.step == "respond") | .subject' <<<"$output")"
  record_completed_step "fresh-$label" "$step"
  st3 trace wait "$run" --for completed --timeout 2m >/dev/null
  state="$(jq --arg label "$label" --arg run "$run" \
    '.scenarios += [{kind: "fresh", label: $label, run: $run, status: "completed"}]' <<<"$state")"
  persist_state
}

fresh_file="$GENERATED/fresh.kdl"
revisable_v1="$GENERATED/revisable-v1.kdl"
revisable_v2="$GENERATED/revisable-v2.kdl"
revisable_v3="$GENERATED/revisable-v3.kdl"
render_fixture fixtures/fresh.kdl "$fresh_file"
render_fixture fixtures/revisable-v1.kdl "$revisable_v1"
render_fixture fixtures/revisable-v2.kdl "$revisable_v2"
render_fixture fixtures/revisable-v3.kdl "$revisable_v3"

first_incarnation="$(wait_for_worker)"
state="$(jq --arg incarnation "$first_incarnation" '.incarnations += [$incarnation]' <<<"$state")"
persist_state

st3 missions publish "$fresh_file" --as "$REQUESTER" >/dev/null
start_fresh one
start_fresh two

st3 missions publish "$revisable_v1" --as "$REQUESTER" >/dev/null
revisable_output="$(st3 missions start eval/work-wake-reliability/revisable \
  --id "revisable-$ST_MISSION_RUN" \
  --workspace "$PWD" \
  --as "$REQUESTER" \
  --json)"
revisable_run="$(jq -er '.mission_run.subject' <<<"$revisable_output")"
initial_step="$(jq -er '.mission_run.steps[] | select(.step == "initial") | .subject' <<<"$revisable_output")"
record_completed_step revision-initial "$initial_step"

revision_one="$(st3 work revise "$revisable_run" "$revisable_v2" \
  --as "$REQUESTER" \
  --reason "exercise the first ordinary live revision wake" \
  --json)"
jq -e '.status == "applied"' <<<"$revision_one" >/dev/null
revision_one_step="$(jq -er '.mission_run.steps[] | select(.step == "revision-one") | .subject' <<<"$revision_one")"
record_completed_step revision-one "$revision_one_step"

revision_two="$(st3 work revise "$revisable_run" "$revisable_v3" \
  --as "$REQUESTER" \
  --reason "exercise the second ordinary live revision wake" \
  --json)"
jq -e '.status == "applied"' <<<"$revision_two" >/dev/null
revision_two_step="$(jq -er '.mission_run.steps[] | select(.step == "revision-two") | .subject' <<<"$revision_two")"
record_completed_step revision-two "$revision_two_step"

st3 terminals signal "$WORKER" hangup >/dev/null
second_incarnation="$(wait_for_worker "$first_incarnation")"
state="$(jq --arg incarnation "$second_incarnation" '.incarnations += [$incarnation]' <<<"$state")"
persist_state

start_fresh after-restart

st3 missions cancel "$revisable_run" \
  --reason "the revision wake scenarios completed" \
  --as "$REQUESTER" >/dev/null
st3 trace wait "$revisable_run" --for cancelled --timeout 2m >/dev/null
st3 missions show "$revisable_run" --json >"$GENERATED/cancelled.json"
jq -e '
  .status == "cancelled"
  and (.steps[] | select(.step == "finalize") | .status) == "completed"
' "$GENERATED/cancelled.json" >/dev/null
state="$(jq --arg run "$revisable_run" \
  '.scenarios += [{kind: "revision", run: $run, revisions: 2, status: "cancelled", finalizer: "completed"}]' <<<"$state")"
persist_state
revisable_run=""
trap - EXIT HUP INT TERM

printf 'fresh assignment, repeated revision, replacement, and cancellation wakes all converged\n'
