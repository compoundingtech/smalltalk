#!/usr/bin/env bash
set -euo pipefail

: "${ST_MISSION_RUN:?ST_MISSION_RUN must identify the eval mission run}"

receipt_deadline_seconds="${WAKE_RECEIPT_DEADLINE_SECONDS:-120}"
turn_stage_deadline_seconds="${WAKE_TURN_STAGE_DEADLINE_SECONDS:-300}"
startup_deadline_seconds="${WAKE_STARTUP_DEADLINE_SECONDS:-180}"
idle_deadline_seconds="${WAKE_IDLE_DEADLINE_SECONDS:-300}"
run_tag="mission-run:$ST_MISSION_RUN"

names=(codex claude pi omp)
declare -A agents peers tokens results
agents[codex]="agent/$ST_MISSION_RUN/wake.codex"
agents[claude]="agent/$ST_MISSION_RUN/wake.claude"
agents[pi]="agent/$ST_MISSION_RUN/wake.pi"
agents[omp]="agent/$ST_MISSION_RUN/wake.omp"
peers[codex]="${agents[claude]}"
peers[claude]="${agents[codex]}"
peers[pi]="${agents[omp]}"
peers[omp]="${agents[pi]}"
tokens[codex]="EMBER"
tokens[claude]="ORBIT"
tokens[pi]="QUARTZ"
tokens[omp]="RIVER"
results[codex]="EMBER+ORBIT"
results[claude]="EMBER+ORBIT"
results[pi]="QUARTZ+RIVER"
results[omp]="QUARTZ+RIVER"

agent_json() {
  st3 agents show "$1" --all --json 2>/dev/null
}

now_ms() {
  # uutils `date` currently ignores the width on `%3N` and emits nine digits. Taking the first
  # thirteen digits of epoch seconds plus nanoseconds is portable across GNU and uutils date.
  date +%s%N | cut -c1-13
}

persist_state() {
  printf '%s\n' "$state" >controller-state.json
}

state="$(jq -n \
  --arg run "$ST_MISSION_RUN" \
  --argjson receipt "$receipt_deadline_seconds" \
  --argjson turn "$turn_stage_deadline_seconds" \
  '{run: $run, deadlines_seconds: {receipt: $receipt, turn_stage: $turn}, agents: {}, phases: {}}')"

wait_for_started() {
  local deadline=$(( $(date +%s) + startup_deadline_seconds ))
  local complete snapshot name agent harness_state
  while [ "$(date +%s)" -lt "$deadline" ]; do
    complete=1
    for name in "${names[@]}"; do
      agent="${agents[$name]}"
      snapshot="$(agent_json "$agent" || true)"
      if jq -e '
        .value.state == "running"
        and .value.reachability == "reachable"
        and (.value.harness_state != null)
        and (.value.harness_state != "unknown")
        and (.value.harness_state != "ended")
      ' <<<"$snapshot" >/dev/null 2>&1; then
        harness_state="$(jq -r '.value.harness_state' <<<"$snapshot")"
        state="$(jq \
          --arg name "$name" --arg id "$agent" --arg pre "$harness_state" \
          --argjson observed "$(jq -r '.snapshot.store_index // 0' <<<"$snapshot")" \
          '.agents[$name] = {id: $id, startup_pre_state: $pre, startup_observed_store_index: $observed}' \
          <<<"$state")"
      else
        complete=0
      fi
    done
    if [ "$complete" -eq 1 ]; then
      persist_state
      return 0
    fi
    sleep 0.5
  done
  printf 'not every harness became reachable before the startup deadline\n' >&2
  return 1
}

wait_for_exact_idle() {
  local deadline=$(( $(date +%s) + idle_deadline_seconds ))
  local complete snapshot name agent
  while [ "$(date +%s)" -lt "$deadline" ]; do
    complete=1
    for name in "${names[@]}"; do
      agent="${agents[$name]}"
      snapshot="$(agent_json "$agent" || true)"
      if jq -e '
        .value.state == "running"
        and .value.reachability == "reachable"
        and .value.harness_state == "idle"
      ' <<<"$snapshot" >/dev/null 2>&1; then
        state="$(jq \
          --arg name "$name" \
          --argjson observed "$(jq -r '.snapshot.store_index // 0' <<<"$snapshot")" \
          '.agents[$name].idle_pre_state = "idle" | .agents[$name].idle_observed_store_index = $observed' \
          <<<"$state")"
      else
        complete=0
      fi
    done
    if [ "$complete" -eq 1 ]; then
      persist_state
      return 0
    fi
    sleep 0.5
  done
  printf 'not every harness reached exact idle before the idle-phase deadline\n' >&2
  return 1
}

render_prompt() {
  local phase=$1 token=$2 peer=$3
  sed \
    -e "s|{{PHASE}}|$phase|g" \
    -e "s|{{TOKEN}}|$token|g" \
    -e "s|{{PEER}}|$peer|g" \
    -e "s|{{RUN}}|$ST_MISSION_RUN|g" \
    prompts/participant.md
}

message_has_kind() {
  local subject=$1 kind=$2
  st3 trace show "$subject" --json --limit 100 2>/dev/null \
    | jq -se --arg kind "$kind" 'any(.kind == $kind)' >/dev/null
}

all_kickoffs_have_kind() {
  local phase=$1 kind=$2 name subject
  for name in "${names[@]}"; do
    subject="$(jq -r --arg phase "$phase" --arg name "$name" '.phases[$phase].kickoffs[$name]' <<<"$state")"
    message_has_kind "$subject" "$kind" || return 1
  done
}

messages_to() {
  local recipient=$1
  jq -s 'add | unique_by(.subject)' \
    <(st3 conversations ls "$recipient" --json) \
    <(st3 conversations ls "$recipient" --archive --json)
}

stage_complete() {
  local phase=$1 stage=$2 name recipient content messages count
  for name in "${names[@]}"; do
    case "$stage" in
      fact)
        recipient="${peers[$name]}"
        content="FACT ${tokens[$name]}"
        ;;
      agreement)
        recipient="${peers[$name]}"
        content="AGREEMENT ${results[$name]}"
        ;;
      result)
        recipient="person/eval-requester"
        content="CONSENSUS ${results[$name]}"
        ;;
      *) return 2 ;;
    esac
    messages="$(messages_to "$recipient")"
    count="$(jq \
      --arg from "${agents[$name]}" \
      --arg run_tag "$run_tag" \
      --arg phase_tag "consensus-wake:$phase" \
      --arg stage_tag "consensus-wake:$stage" \
      --arg content "$content" \
      '[.[] | select(
        .from == $from
        and (.tags | index($run_tag))
        and (.tags | index($phase_tag))
        and (.tags | index($stage_tag))
        and (.content | contains($content))
      )] | length' <<<"$messages")"
    [ "$count" -eq 1 ] || return 1
  done
}

wait_until() {
  local label=$1 seconds=$2
  shift 2
  local deadline=$(( $(date +%s) + seconds ))
  while [ "$(date +%s)" -lt "$deadline" ]; do
    if "$@"; then
      return 0
    fi
    sleep 1
  done
  printf '%s did not complete within %ss\n' "$label" "$seconds" >&2
  return 1
}

run_phase() {
  local phase=$1 name agent body kickoff sent_at delivered_at stage
  sent_at="$(now_ms)"
  state="$(jq --arg phase "$phase" --argjson sent "$sent_at" \
    '.phases[$phase] = {sent_at_unix_ms: $sent, kickoffs: {}, stages: {}}' <<<"$state")"
  for name in "${names[@]}"; do
    agent="${agents[$name]}"
    body="$(render_prompt "$phase" "${tokens[$name]}" "${peers[$name]}")"
    kickoff="$(st3 conversations send "$agent" --from person/eval-requester \
      --subject "Cross-harness consensus: $phase" \
      --tags "$run_tag,consensus-wake:$phase,consensus-wake:kickoff" \
      --body "$body" --json | jq -er .subject)"
    state="$(jq --arg phase "$phase" --arg name "$name" --arg kickoff "$kickoff" \
      '.phases[$phase].kickoffs[$name] = $kickoff' <<<"$state")"
  done
  persist_state

  wait_until "$phase native consumption receipt" "$receipt_deadline_seconds" \
    all_kickoffs_have_kind "$phase" message.delivered
  delivered_at="$(now_ms)"
  state="$(jq --arg phase "$phase" --argjson at "$delivered_at" \
    '.phases[$phase].all_delivered_at_unix_ms = $at' <<<"$state")"
  persist_state

  for stage in fact agreement result; do
    wait_until "$phase $stage stage" "$turn_stage_deadline_seconds" \
      stage_complete "$phase" "$stage"
    state="$(jq --arg phase "$phase" --arg stage "$stage" --argjson at "$(now_ms)" \
      '.phases[$phase].stages[$stage] = {completed_at_unix_ms: $at}' <<<"$state")"
    persist_state
  done
}

wait_for_started
run_phase startup
wait_for_exact_idle
run_phase idle

printf 'all four harnesses completed startup and exact-idle consensus\n'
