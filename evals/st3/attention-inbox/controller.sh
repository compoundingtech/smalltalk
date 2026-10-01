#!/usr/bin/env bash
set -Eeuo pipefail
exec >controller.log 2>&1

readonly REQUESTER=person/eval-requester
readonly RUN_SUFFIX=${ST_MISSION_RUN//\//-}
readonly REVIEWER=person/eval-attention

standing_run=""
human_run=""
revision_run=""
planning_session=""
message_subject=""
human_owner=""
proposal_subject=""
proposal_hash=""

cancel_run() {
  local run=$1
  [[ -z "$run" ]] && return 0
  printf 'version 2\nmission-run "%s" { cancellation "eval-finished" { reason "the attention inbox eval finished" } }\n' "$run" \
    | st3 publish - --as "$REQUESTER" >/dev/null 2>&1 || true
}

cleanup() {
  if [[ -n "$planning_session" ]]; then
    st3 launch cancel "$planning_session" --as "$REVIEWER" \
      --reason "the attention inbox eval finished" >/dev/null 2>&1 || true
  fi
  if [[ -n "$message_subject" ]]; then
    st3 conversations archive "$message_subject" --as "$REVIEWER" >/dev/null 2>&1 || true
  fi
  if [[ -n "$human_owner" ]]; then
    st3 review reject "$human_owner" --actor "$REVIEWER" \
      --reason "the attention inbox eval finished" >/dev/null 2>&1 || true
  fi
  if [[ -n "$proposal_subject" ]]; then
    st3 work revision cancel "$proposal_subject" --as "$REVIEWER" \
      --reason "the attention inbox eval finished" >/dev/null 2>&1 || true
  fi
  cancel_run "$revision_run"
  cancel_run "$human_run"
  cancel_run "$standing_run"
}
trap cleanup EXIT HUP INT TERM

await_kind_count() {
  local expected=$1
  local output=$2
  local count
  for _attempt in $(seq 1 200); do
    st3 --json attention ls --as "$REVIEWER" >"$output"
    count=$(jq -r 'length' "$output")
    if [[ "$count" == "$expected" ]]; then
      return 0
    fi
    sleep 0.1
  done
  return 1
}

st3 --json mission start eval/attention-inbox/standing \
  --id "eval/attention-inbox/standing/${RUN_SUFFIX}" \
  --workspace "$PWD" \
  --as "$REQUESTER" >standing.json
standing_run=$(jq -er '.mission_run.subject' standing.json)
owner_id="${standing_run#mission-run/}/owner"
owner="agent/${owner_id}"
st3 trace wait "$owner" --for running --timeout 1m >/dev/null

st3 --json mission start eval/attention-inbox/human \
  --id "eval/attention-inbox/human/${RUN_SUFFIX}" \
  --workspace "$PWD" \
  --as "$REQUESTER" >human.json
human_run=$(jq -er '.mission_run.subject' human.json)
human_owner=$(jq -er '.mission_run.steps[] | select(.step == "approve") | .subject' human.json)

st3 --json mission start eval/attention-inbox/revision \
  --id "eval/attention-inbox/revision/${RUN_SUFFIX}" \
  --workspace "$PWD" \
  --input "owner=${owner_id}" \
  --as "$owner" >revision.json
revision_run=$(jq -er '.mission_run.subject' revision.json)
revision_step=$(jq -er '.mission_run.steps[] | select(.step == "work") | .subject' revision.json)
st3 trace wait "$revision_step" --for ready --timeout 1m >/dev/null
st3 work claim "$revision_step" --as "$owner" >/dev/null
st3 --json work revise "$revision_run" revision.kdl \
  --as "$owner" \
  --reason "the model-free eval needs one revision approval" >revision-proposal.json
proposal_subject=$(jq -er '.proposal.subject' revision-proposal.json)
proposal_hash=$(jq -er '.proposal.preview_hash' revision-proposal.json)

# A workspace that is a file refuses the planner's render, so the model-free planner never starts.
printf '%s\n' 'This file blocks the model-free planner.' >blocked-planner
st3 --json launch start \
  --id eval/attention-inbox/planned \
  request.md \
  --workspace "$PWD/blocked-planner" \
  --as "$REVIEWER" >planning.json
planning_session=$(jq -er '.id' planning.json)
planning_subject=$(jq -er '.subject' planning.json)
planner=$(jq -er '.planner' planning.json)
st3 launch submit "$planning_session" \
  --markdown planned.md \
  --kdl planned.kdl \
  --as "$planner" >/dev/null
st3 --json launch preview "$planning_session" >planning-preview.json

st3 --json message send "$REVIEWER" \
  --from "$REQUESTER" \
  --subject "Read the attention inbox proof" \
  -m "This message stays in conversations, never in the attention inbox." >message.json
message_subject=$(jq -er '.subject' message.json)

await_kind_count 3 attention.json
st3 --json attention ls >all-attention.json
jq -e \
  --arg person "$REVIEWER" \
  --arg human "$human_owner" \
  --arg revision "$proposal_subject" \
  --arg planning "$planning_subject" \
  --arg message "$message_subject" '
    length == 3
    and all(.[]; .person == $person and (.actions | length > 0))
    and ([.[].kind] | sort) == [
      "human-gate",
      "planning-approval",
      "revision-approval"
    ]
    and (map(select(.kind == "human-gate"))[0].subject == $human)
    and (map(select(.kind == "revision-approval"))[0].subject == $revision)
    and (map(select(.kind == "planning-approval"))[0].subject == $planning)
    and all(.[]; .subject != $message)
  ' attention.json >/dev/null
jq -e --arg person "$REVIEWER" '
  map(select(.person == $person)) | length == 3
' all-attention.json >/dev/null

st3 attention ls --as "$REVIEWER" >attention.txt
grep -F '3 waiting' attention.txt >/dev/null
grep -F '[human-gate]' attention.txt >/dev/null
grep -F '[planning-approval]' attention.txt >/dev/null
grep -F '[revision-approval]' attention.txt >/dev/null
if grep -E '\[(unread-message|fault)\]' attention.txt >/dev/null; then
  exit 1
fi

st3 review approve "$human_owner" --actor "$REVIEWER" \
  --reason "the model-free human gate is correct" >/dev/null
st3 work revision approve "$proposal_subject" "$proposal_hash" \
  --as "$REVIEWER" >/dev/null
proposal_subject=""
st3 launch cancel "$planning_session" --as "$REVIEWER" \
  --reason "the model-free planning item is complete" >/dev/null
planning_session=""

st3 conversations read "$message_subject" --as "$REVIEWER" >/dev/null

await_kind_count 0 empty-attention.json
st3 review ls --as "$REVIEWER" --json \
  | jq -e 'length == 0' >/dev/null
st3 conversations archive "$message_subject" --as "$REVIEWER" >/dev/null
message_subject=""

cancel_run "$standing_run"
standing_run=""
printf '%s\n' ATTENTION-INBOX-GREEN >result.txt
