#!/usr/bin/env bash
set -Eeuo pipefail

: "${ST_MISSION_RUN:?ST_MISSION_RUN must identify the eval mission run}"

readonly REQUESTER=person/eval-requester
readonly OPERATOR=person/eval-operator
readonly WORKER=agent/eval/seat-queue/worker
readonly GENERATED="$PWD/generated"
readonly STATE="$PWD/controller-state.json"

started_runs=()

cleanup() {
  local run
  for run in "${started_runs[@]}"; do
    st3 missions cancel "$run" \
      --reason "the seat queue controller is cleaning up after an early exit" \
      --as "$REQUESTER" >/dev/null 2>&1 || true
  done
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

mkdir -p "$GENERATED"

state="$(jq -n --arg worker "$WORKER" --arg operator "$OPERATOR" \
  '{worker: $worker, operator: $operator, runs: {}, steps: {}, claims: [], checkpoints: []}')"
persist_state() {
  printf '%s\n' "$state" >"$STATE"
}
persist_state

fail() {
  printf 'seat queue controller: %s\n' "$*" >&2
  state="$(jq --arg reason "$*" '.failure = $reason' <<<"$state")"
  persist_state
  exit 1
}

now_ms() {
  local micros=${EPOCHREALTIME/./}
  printf '%s\n' "${micros:0:-3}"
}

checkpoint() {
  local name=$1
  state="$(jq --arg name "$name" --argjson at "$(now_ms)" \
    '.checkpoints += [{name: $name, at_unix_ms: $at}]' <<<"$state")"
  persist_state
}

wait_for_worker() {
  local deadline=$((SECONDS + 300)) snapshot
  while (( SECONDS < deadline )); do
    snapshot="$(st3 agents show "$WORKER" --all --json 2>/dev/null || true)"
    if jq -e '
      .value.state == "running"
      and .value.reachability == "reachable"
      and .value.harness_state == "idle"
    ' <<<"$snapshot" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.5
  done
  fail "the seat did not reach exact idle"
}

render_fixture() {
  local source=$1 destination=$2
  sed "s|{{WORKER}}|$WORKER|g" "$source" >"$destination"
}

step_state() {
  st3 work show "$1" --json | jq -er '.value.state'
}

wait_for_step() {
  local subject=$1 pattern=$2 timeout=${3:-300} deadline status
  deadline=$((SECONDS + timeout))
  while (( SECONDS < deadline )); do
    status="$(step_state "$subject" 2>/dev/null || true)"
    if [[ "$status" =~ ^($pattern)$ ]]; then
      return 0
    fi
    sleep 0.2
  done
  fail "$subject did not reach $pattern; it is ${status:-unknown}"
}

queue_snapshot() {
  local name=$1
  st3 agents queue "$WORKER" --json >"$GENERATED/queue-$name.json"
  checkpoint "queue-$name"
}

queue_runs() {
  jq -c '[.value.runs[].mission_run_id]' "$GENERATED/queue-$1.json"
}

queue_next() {
  jq -r '.value.next_work_id // ""' "$GENERATED/queue-$1.json"
}

# The seat's four assigned steps. A claim is observed as the first state after
# `ready`; the controller never claims or completes seat work itself.
seat_steps=()
declare -A claimed_steps=()

expect_next_claim() {
  local expected=$1 deadline=$((SECONDS + 300)) subject status
  while (( SECONDS < deadline )); do
    for subject in "${seat_steps[@]}"; do
      [[ -n "${claimed_steps[$subject]:-}" ]] && continue
      status="$(step_state "$subject" 2>/dev/null || true)"
      if [[ "$status" =~ ^(claimed|working|verifying|completed)$ ]]; then
        claimed_steps[$subject]=1
        state="$(jq --arg subject "$subject" --argjson at "$(now_ms)" \
          '.claims += [{subject: $subject, observed_at_unix_ms: $at}]' <<<"$state")"
        persist_state
        if [[ "$subject" != "$expected" ]]; then
          fail "the seat claimed $subject while the queue's next work was $expected"
        fi
        return 0
      fi
    done
    sleep 0.2
  done
  fail "the seat did not claim $expected"
}

start_run() {
  local label=$1 output run
  output="$(st3 missions start "eval/seat-queue/$label" \
    --id "$label-$ST_MISSION_RUN" \
    --workspace "$PWD" \
    --as "$REQUESTER" \
    --json)"
  run="$(jq -er '.mission_run.subject' <<<"$output")"
  started_runs+=("$run")
  state="$(jq --arg label "$label" --arg run "$run" '.runs[$label] = $run' <<<"$state")"
  while IFS=$'\t' read -r step subject; do
    state="$(jq --arg key "$label/$step" --arg subject "$subject" \
      '.steps[$key] = $subject' <<<"$state")"
  done < <(jq -r '.mission_run.steps[] | [.step, .subject] | @tsv' <<<"$output")
  persist_state
}

step_subject() {
  jq -er --arg key "$1" '.steps[$key]' <<<"$state"
}

run_subject() {
  jq -er --arg label "$1" '.runs[$label]' <<<"$state"
}

for label in alpha bravo charlie; do
  render_fixture "fixtures/$label.kdl" "$GENERATED/$label.kdl"
done

wait_for_worker
checkpoint seat-idle

for label in alpha bravo charlie; do
  st3 missions publish "$GENERATED/$label.kdl" --as "$REQUESTER" >/dev/null
done

# Start order is the default queue order: alpha, then bravo, then charlie.
start_run alpha
start_run bravo
start_run charlie
checkpoint runs-started

alpha="$(run_subject alpha)"
bravo="$(run_subject bravo)"
charlie="$(run_subject charlie)"
alpha_draft="$(step_subject alpha/draft)"
alpha_sign_off="$(step_subject alpha/sign-off)"
alpha_publish="$(step_subject alpha/publish)"
bravo_work="$(step_subject bravo/work)"
charlie_work="$(step_subject charlie/work)"
seat_steps=("$alpha_draft" "$alpha_publish" "$bravo_work" "$charlie_work")

queue_snapshot started
[[ "$(queue_runs started)" == "$(jq -cn --arg a "$alpha" --arg b "$bravo" --arg c "$charlie" '[$a, $b, $c]')" ]] \
  || fail "the seat queue did not keep start order: $(queue_runs started)"

# The seat takes the head run's ready step first.
expect_next_claim "$alpha_draft"
checkpoint alpha-draft-claimed

queue_snapshot before-move
[[ "$(queue_next before-move)" == "$bravo_work" ]] \
  || fail "before the move the next work was $(queue_next before-move), not $bravo_work"

# A person moves the last run ahead of bravo while the seat holds alpha's draft.
st3 agents queue move "$WORKER" "$charlie" --before "$bravo" \
  --reason "charlie's notes are needed before bravo's" \
  --as "$OPERATOR" --json >"$GENERATED/move.json"
checkpoint queue-moved
held_during_move="$(step_state "$alpha_draft")"
[[ "$held_during_move" =~ ^(claimed|working)$ ]] \
  || fail "the move did not land while alpha's draft was held (it was $held_during_move); rerun the eval"

queue_snapshot after-move
[[ "$(queue_runs after-move)" == "$(jq -cn --arg a "$alpha" --arg b "$bravo" --arg c "$charlie" '[$a, $c, $b]')" ]] \
  || fail "the move did not reorder the queue: $(queue_runs after-move)"
[[ "$(queue_next after-move)" == "$charlie_work" ]] \
  || fail "after the move the next work was $(queue_next after-move), not $charlie_work"
jq -e --arg held "$alpha_draft" '.value.current_work_ids == [$held]' "$GENERATED/queue-after-move.json" >/dev/null \
  || fail "the move changed the seat's held work"

wait_for_step "$alpha_draft" completed
checkpoint alpha-draft-completed

# Alpha is still the head run, but its next seat step waits for sign-off, so the
# seat passes over it.
expect_next_claim "$charlie_work"
checkpoint charlie-claimed
queue_snapshot passed-over
jq -e --arg alpha "$alpha" '.value.runs[0].mission_run_id == $alpha and .value.runs[0].state == "waiting"' \
  "$GENERATED/queue-passed-over.json" >/dev/null \
  || fail "alpha was not the waiting head run when charlie was claimed"

# Release alpha's gate while the seat still holds charlie's work.
st3 attention approve "$alpha_sign_off" \
  --as "$REQUESTER" \
  --reason "the alpha draft is signed off" >/dev/null
checkpoint sign-off-approved
wait_for_step "$alpha_publish" 'ready|claimed|working|verifying|completed' 60
checkpoint alpha-publish-ready
held_when_ready="$(step_state "$charlie_work")"
[[ "$held_when_ready" =~ ^(claimed|working)$ ]] \
  || fail "alpha became ready after the seat released charlie's work (it was $held_when_ready); rerun the eval"
queue_snapshot head-ready
[[ "$(queue_next head-ready)" == "$alpha_publish" ]] \
  || fail "the ready head run was not next: $(queue_next head-ready)"

wait_for_step "$charlie_work" completed
checkpoint charlie-completed

# The seat returns to the head run before bravo, which has been ready all along.
expect_next_claim "$alpha_publish"
checkpoint alpha-publish-claimed
wait_for_step "$alpha_publish" completed
checkpoint alpha-publish-completed

expect_next_claim "$bravo_work"
checkpoint bravo-claimed
wait_for_step "$bravo_work" completed
checkpoint bravo-completed

for run in "$alpha" "$bravo" "$charlie"; do
  st3 trace wait "$run" --for completed --timeout 2m >/dev/null
done
queue_snapshot finished
jq -e '.value.runs == [] and .value.current_work_ids == []' "$GENERATED/queue-finished.json" >/dev/null \
  || fail "the seat queue did not empty after every run completed"
checkpoint runs-completed

wait_for_worker
state="$(jq '.result = "passed"' <<<"$state")"
persist_state
started_runs=()
trap - EXIT HUP INT TERM

printf 'the seat followed the moved queue, passed over the waiting head run, and returned to it once ready\n'
