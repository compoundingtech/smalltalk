#!/usr/bin/env bash
set -Eeuo pipefail

: "${ST_MISSION_RUN:?ST_MISSION_RUN must identify the eval mission run}"

readonly REQUESTER=person/eval-requester
readonly AGENT="agent/$ST_MISSION_RUN/compact.codex"
readonly RUN_TAG="mission-run:$ST_MISSION_RUN"
readonly NONCE="${ST_MISSION_RUN##*/}"
readonly STATE="$PWD/controller-state.json"

state="$(jq -n --arg agent "$AGENT" --arg nonce "$NONCE" \
  '{agent: $agent, nonce: $nonce, messages: {}, compaction: {}}')"
persist_state() { printf '%s\n' "$state" >"$STATE"; }
persist_state

agent_json() { st3 agents show "$AGENT" --all --json 2>/dev/null; }

wait_until() {
  local label=$1 seconds=$2
  shift 2
  local deadline=$((SECONDS + seconds))
  while (( SECONDS < deadline )); do
    if "$@"; then return 0; fi
    sleep 0.5
  done
  printf '%s did not complete within %ss\n' "$label" "$seconds" >&2
  return 1
}

is_harness_state() {
  local wanted=$1 snapshot
  snapshot="$(agent_json || true)"
  jq -e --arg wanted "$wanted" '
    .value.state == "running"
    and .value.reachability == "reachable"
    and .value.harness_state == $wanted
  ' <<<"$snapshot" >/dev/null 2>&1
}

target_work_claimed() {
  st3 missions show "mission-run/$ST_MISSION_RUN" --json 2>/dev/null \
    | jq -e --arg agent "$AGENT" '
        any(.steps[];
          .step == "message-target"
          and (.status == "claimed" or .status == "working")
          and .claimant == $agent
        )
      ' >/dev/null 2>&1
}

message_has_lifecycle() {
  local subject=$1
  st3 trace show "$subject" --json --limit 100 2>/dev/null \
    | jq -se '
        map(.kind) as $kinds
        | ($kinds | index("message.staged")) != null
        and ($kinds | index("message.delivered")) != null
        and ($kinds | index("message.read")) != null
      ' >/dev/null
}

requester_has() {
  local marker=$1
  jq -s 'add | unique_by(.subject)' \
    <(st3 conversations ls "$REQUESTER" --json) \
    <(st3 conversations ls "$REQUESTER" --archive --json) \
    | jq -e --arg from "$AGENT" --arg marker "$marker" --arg tag "$RUN_TAG" '
        any(.from == $from and (.tags | index($tag)) and (.content | contains($marker)))
      ' >/dev/null
}

sent_subject=""
send_request() {
  local title=$1 body=$2 stage=$3 subject
  subject="$(st3 conversations send "$AGENT" --from "$REQUESTER" \
    --subject "$title" --tags "$RUN_TAG,compact-delivery:$stage" \
    --body "$body" --json | jq -er .subject)"
  state="$(jq --arg stage "$stage" --arg subject "$subject" \
    '.messages[$stage] = $subject' <<<"$state")"
  persist_state
  sent_subject="$subject"
}

latest_compactions() {
  st3 trace show "$AGENT" --json --limit 500 2>/dev/null \
    | jq -sr '[.[] | select(
        .kind == "harness.usage"
        and .body.fields.semantics == "context_occupancy"
      )] | last | .body.fields.compactions // 0'
}

latest_compaction_trigger() {
  st3 trace show "$AGENT" --json --limit 500 2>/dev/null \
    | jq -sr '[.[] | select(
        .kind == "harness.usage"
        and .body.fields.semantics == "context_occupancy"
      )] | last | .body.fields.last_compaction_trigger // ""'
}

compaction_advanced() {
  local current trigger
  current="$(latest_compactions)"
  trigger="$(latest_compaction_trigger)"
  [[ "$current" -gt "$before_compactions" && "$trigger" == manual ]]
}

wait_until "graph-backed target claim" 300 target_work_claimed
wait_until "initial exact idle" 300 is_harness_state idle

send_request \
  "Working-turn anchor" \
  "Run the shell command sleep 20. After it finishes, send $REQUESTER exactly one Small Talk message containing ANCHOR-DONE $NONCE and tagged $RUN_TAG. Do not modify files." \
  anchor
anchor="$sent_subject"
wait_until "anchor native lifecycle" 180 message_has_lifecycle "$anchor"
wait_until "Codex working state" 90 is_harness_state working

send_request \
  "Working-turn steer" \
  "Send $REQUESTER exactly one Small Talk message containing STEER-SEEN $NONCE and tagged $RUN_TAG. Do not modify files." \
  steer
steer="$sent_subject"
wait_until "steered message lifecycle" 180 message_has_lifecycle "$steer"
wait_until "steered response" 300 requester_has "STEER-SEEN $NONCE"
wait_until "anchor response" 300 requester_has "ANCHOR-DONE $NONCE"
wait_until "pre-compaction exact idle" 300 is_harness_state idle

before_compactions="$(latest_compactions)"
state="$(jq --argjson before "$before_compactions" \
  '.compaction.before = $before' <<<"$state")"
persist_state

st3 terminals send "$AGENT" /compact --json >compact-control.json
wait_until "manual compaction graph edge" 300 compaction_advanced
wait_until "post-compaction exact idle" 300 is_harness_state idle
after_compactions="$(latest_compactions)"
state="$(jq --argjson after "$after_compactions" \
  '.compaction.after = $after | .compaction.trigger = "manual" | .compaction.control = "compact-control.json"' <<<"$state")"
persist_state

send_request \
  "Post-compaction delivery" \
  "Send $REQUESTER exactly one Small Talk message containing POST-COMPACT-SEEN $NONCE and tagged $RUN_TAG. Then complete your claimed message-target work step in the graph. Do not modify files." \
  post_compact
post="$sent_subject"
wait_until "post-compaction native lifecycle" 180 message_has_lifecycle "$post"
wait_until "post-compaction response" 300 requester_has "POST-COMPACT-SEEN $NONCE"
wait_until "final exact idle" 300 is_harness_state idle

state="$(jq '.result = "passed"' <<<"$state")"
persist_state
printf 'working-turn steer and post-/compact delivery both passed\n'
