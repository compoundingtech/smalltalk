#!/usr/bin/env bash
set -euo pipefail

: "${ST_MISSION_RUN:?ST_MISSION_RUN must identify the judged mission run}"
test -s controller-state.json

run_tag="mission-run:$ST_MISSION_RUN"
names=(codex claude pi omp)
phases=(startup idle)
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

jq -e '
  def milliseconds:
    if . > 1000000000000000 then (. / 1000000) else . end;
  .run != null
  and (.phases.startup.stages.result.completed_at_unix_ms != null)
  and (.phases.idle.stages.result.completed_at_unix_ms != null)
  and ([.agents[].idle_pre_state] | all(. == "idle"))
  and (((.phases.startup.all_delivered_at_unix_ms | milliseconds) - (.phases.startup.sent_at_unix_ms | milliseconds)) <= (.deadlines_seconds.receipt * 1000))
  and (((.phases.idle.all_delivered_at_unix_ms | milliseconds) - (.phases.idle.sent_at_unix_ms | milliseconds)) <= (.deadlines_seconds.receipt * 1000))
' controller-state.json >/dev/null

messages_to() {
  local recipient=$1
  jq -s 'add | unique_by(.subject)' \
    <(st3 conversations ls "$recipient" --json) \
    <(st3 conversations ls "$recipient" --archive --json)
}

require_lifecycle() {
  local subject=$1 claims
  claims="$(st3 trace show "$subject" --json --limit 100 | jq -s '.')"
  jq -e 'map(.kind) | index("message.delivered") != null and index("message.read") != null' \
    <<<"$claims" >/dev/null
}

require_one() {
  local recipient=$1 from=$2 phase=$3 stage=$4 content=$5 messages
  messages="$(messages_to "$recipient")"
  jq -er \
    --arg from "$from" --arg run_tag "$run_tag" \
    --arg phase_tag "consensus-wake:$phase" \
    --arg stage_tag "consensus-wake:$stage" \
    --arg content "$content" '
      [.[] | select(
        .from == $from
        and (.tags | index($run_tag))
        and (.tags | index($phase_tag))
        and (.tags | index($stage_tag))
        and (.content | contains($content))
      )] | if length == 1 then .[0].subject else error("expected exactly one protocol message") end
    ' <<<"$messages"
}

for phase in "${phases[@]}"; do
  for name in "${names[@]}"; do
    kickoff="$(jq -er --arg phase "$phase" --arg name "$name" '.phases[$phase].kickoffs[$name]' controller-state.json)"
    require_lifecycle "$kickoff"
    fact="$(require_one "${peers[$name]}" "${agents[$name]}" "$phase" fact "FACT ${tokens[$name]}")"
    agreement="$(require_one "${peers[$name]}" "${agents[$name]}" "$phase" agreement "AGREEMENT ${results[$name]}")"
    result="$(require_one person/eval-requester "${agents[$name]}" "$phase" result "CONSENSUS ${results[$name]}")"
    require_lifecycle "$fact"
    require_lifecycle "$agreement"
    # The requester is a human mailbox, not a provider seat. Its report must exist exactly once,
    # but no harness can honestly publish a provider-consumption receipt for the human.
    test -n "$result"
  done
done

echo "PASS: Codex, Claude, Pi, and OMP consumed startup and idle messages and completed both paired consensus protocols"
