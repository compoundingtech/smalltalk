#!/usr/bin/env bash
set -Eeuo pipefail

readonly EVAL_ROOT=$PWD
readonly TARGET_WORKSPACE="$EVAL_ROOT/planner-workspace"
readonly REQUEST="$EVAL_ROOT/request.md"
readonly MISSION_ID=${PLANNED_MISSION:?PLANNED_MISSION is required}
readonly MISSION_SUBJECT="mission/$MISSION_ID"
readonly REQUESTER="person/eval-requester"

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

workspace_digest() {
  find "$TARGET_WORKSPACE" -type f -print0 \
    | sort -z \
    | xargs -0 sha256sum \
    | sha256sum \
    | cut -d' ' -f1
}

assert_mission_is_unpublished() {
  st3 --json status "$MISSION_SUBJECT" > "$EVAL_ROOT/mission-status-before.json"
  jq -e '
    .subjects | length == 1
    and .[0].actual == null
    and (.[0].claims | length) == 0
  ' "$EVAL_ROOT/mission-status-before.json" >/dev/null
}

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

before_digest=$(workspace_digest)

st3 --json launch start \
  --id "$MISSION_ID" \
  "$REQUEST" \
  --workspace "$TARGET_WORKSPACE" \
  --as "$REQUESTER" \
  --model gpt-5.6-sol \
  --effort medium \
  > "$EVAL_ROOT/started.json"

session_id=$(jq -er '.id' "$EVAL_ROOT/started.json")
session_subject=$(jq -er '.subject' "$EVAL_ROOT/started.json")
planner=$(jq -er '.planner' "$EVAL_ROOT/started.json")

assert_mission_is_unpublished
wait_for_event \
  "$session_subject" \
  "planning-session.candidate-submitted" \
  "$EVAL_ROOT/planning-events.jsonl"
assert_mission_is_unpublished

variant=$(st3 --json launch show "$session_id" | jq -er '.candidate.variant')
st3 --json launch preview "$session_id" --variant "$variant" > "$EVAL_ROOT/preview.json"
preview_hash=$(jq -er '.preview.hash' "$EVAL_ROOT/preview.json")
jq -e '
  .status == "review"
  and (.preview.graph | contains("inspect [root]"))
  and (.preview.graph | contains("verify [after inspect]"))
  and (.preview.diff | contains("mission/"))
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
' "$EVAL_ROOT/approved.json" >/dev/null

st3 --json status "$MISSION_SUBJECT" > "$EVAL_ROOT/mission-status-after.json"
jq -e --arg mission "$MISSION_ID" '
  .subjects | length == 1
  and .[0].actual.id == $mission
  and .[0].actual.state == "ready"
' "$EVAL_ROOT/mission-status-after.json" >/dev/null

st3 --json status > "$EVAL_ROOT/all-status-after.json"
jq -e --arg mission "$MISSION_SUBJECT" '
  [
    .subjects[]
    | select(.subject | startswith("mission-run/"))
    | select((.actual.mission // "") == $mission)
  ] | length == 0
' "$EVAL_ROOT/all-status-after.json" >/dev/null

st3 --json inspect "$MISSION_SUBJECT" > "$EVAL_ROOT/mission-inspect.json"
jq -e '
  ([.recent_claims[] | select(.kind == "mission.published")] | length) == 1
' "$EVAL_ROOT/mission-inspect.json" >/dev/null
st3 --json inspect "$session_subject" > "$EVAL_ROOT/planning-session-inspect.json"
jq -e '
  ([.recent_claims[] | select(.kind == "planning-session.approved")] | length) == 1
' "$EVAL_ROOT/planning-session-inspect.json" >/dev/null

markdown_ref=$(jq -er \
  '.recent_claims[] | select(.kind == "planning-session.approved") | .body.fields.markdown' \
  "$EVAL_ROOT/planning-session-inspect.json")
kdl_ref=$(jq -er \
  '.recent_claims[] | select(.kind == "planning-session.approved") | .body.fields.kdl' \
  "$EVAL_ROOT/planning-session-inspect.json")
jq -e --arg markdown "$markdown_ref" --arg kdl "$kdl_ref" '
  .candidate.markdown == $markdown
  and .candidate.kdl == $kdl
' "$EVAL_ROOT/approved.json" >/dev/null
st3 documents get "$markdown_ref" --output "$EVAL_ROOT/approved-mission.md" >/dev/null
st3 documents get "$kdl_ref" --output "$EVAL_ROOT/approved-mission.kdl" >/dev/null
test -s "$EVAL_ROOT/approved-mission.md"
test -s "$EVAL_ROOT/approved-mission.kdl"

st3 trace wait "$planner" --for stopped --timeout 2m >/dev/null

after_digest=$(workspace_digest)
[[ "$before_digest" == "$after_digest" ]]

printf '%s\n' "$preview_hash" > "$EVAL_ROOT/approved-preview-hash.txt"
printf '%s\n' PLANNING-MODE-GREEN > "$EVAL_ROOT/result.txt"
