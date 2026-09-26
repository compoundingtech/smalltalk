#!/usr/bin/env bash
set -Eeuo pipefail
exec >controller.log 2>&1

readonly REQUESTER=person/eval-requester
readonly OWNER=agent/eval/review-escalation/standing/owner
standing_run=""
work_run=""

cancel_run() {
  local run=$1
  [[ -z "$run" ]] && return 0
  printf 'version 2\nmission-run "%s" { cancellation "eval-finished" { reason "the review escalation eval finished" } }\n' "$run" \
    | st3 publish - --as "$REQUESTER" >/dev/null 2>&1 || true
}

cleanup() {
  cancel_run "$work_run"
  cancel_run "$standing_run"
}
trap cleanup EXIT HUP INT TERM

st3 publish base.kdl --as "$REQUESTER" >/dev/null
st3 --json claim resource/eval/review-escalation/source resource.observed \
  --actor "$REQUESTER" \
  --field kind=vcs.pull-request \
  --field state=ready >source.json
source_claim=$(jq -er '.id' source.json)

st3 --json mission start eval/review-escalation/standing \
  --id eval/review-escalation/standing \
  --workspace "$PWD" \
  --as "$REQUESTER" >standing.json
standing_run=$(jq -er '.mission_run.subject' standing.json)
st3 trace wait "$OWNER" --for running --timeout 1m >/dev/null

st3 --json mission start eval/review-escalation/work \
  --id "eval/review-escalation/work/${ST_MISSION_RUN}" \
  --workspace "$PWD" \
  --input "source=resource/eval/review-escalation/source@${source_claim}" \
  --as "$OWNER" >work.json
work_run=$(jq -er '.mission_run.subject' work.json)
old_generation=$(jq -er '.mission_run.generation' work.json)

st3 claim "resource/mission-run/${work_run#mission-run/}/review" resource.observed \
  --actor "$REQUESTER" \
  --field kind=human.review \
  --field target=resource/eval/review-escalation/source >/dev/null
st3 trace wait "$work_run" --for running --timeout 1m >/dev/null

route=$(st3 --json mission show "$work_run" | jq -er '.steps[] | select(.step == "route") | .subject')
st3 trace wait "$route" --for ready --timeout 1m >/dev/null
st3 work claim "$route" --as "$OWNER" >/dev/null

st3 --json work revise "$work_run" escalation.kdl \
  --as "$OWNER" \
  --reason "the owner selected the human review route" >revised.json
new_generation=$(jq -er '.mission_run.generation' revised.json)
[[ "$new_generation" != "$old_generation" ]]

route=$(jq -er '.mission_run.steps[] | select(.step == "route") | .subject' revised.json)
st3 work claim "$route" --as "$OWNER" >/dev/null
st3 work complete "$route" --as "$OWNER" --summary "Nathan must review this result" >/dev/null

human=$(st3 --json mission show "$work_run" | jq -er '.steps[] | select(.step == "human-review") | .subject')
st3 trace wait "$human" --for ready --timeout 1m >/dev/null
review_list_ready=false
for attempt in $(seq 1 100); do
  st3 review ls --as person/nathan --json >pending-reviews.json
  if jq -e --arg owner "$human" 'map(select(.owner == $owner)) | length == 1' pending-reviews.json >/dev/null; then
    review_list_ready=true
    break
  fi
  sleep 0.1
done
[[ "$review_list_ready" == "true" ]]
jq -e --arg owner "$human" --arg run "$work_run" --arg source "$source_claim" '
  map(select(.owner == $owner)) as $reviews
  | $reviews | length == 1
  and $reviews[0].mission == "mission/eval/review-escalation/work"
  and $reviews[0].mission_run == $run
  and $reviews[0].step == "human-review"
  and $reviews[0].reviewer == "person/nathan"
  and $reviews[0].question == "Approve human-review?"
  and $reviews[0].review_targets == [
    "resource/eval/review-escalation/source@" + $source,
    "resource/mission-run/" + ($run | sub("^mission-run/"; "")) + "/review"
  ]
  and $reviews[0].decisions == ["approved", "rejected"]
  and $reviews[0].attempt == 1
' pending-reviews.json >/dev/null
review_requested=false
for attempt in $(seq 1 100); do
  if st3 review approve "$human" --actor person/nathan --reason "the model-free review route is correct" >/dev/null 2>&1; then
    review_requested=true
    break
  fi
  sleep 0.1
done
[[ "$review_requested" == "true" ]]
st3 trace wait "$work_run" --for completed --timeout 1m >/dev/null
st3 review ls --as person/nathan --json \
  | jq -e --arg owner "$human" 'map(select(.owner == $owner)) | length == 0' >/dev/null

st3 --json work revision generations "$work_run" >generations.json
jq -e --arg old "$old_generation" --arg new "$new_generation" '
  length == 2
  and .[0].subject == $old
  and .[0].status == "superseded"
  and .[1].subject == $new
  and .[1].predecessor == $old
' generations.json >/dev/null

printf '%s\n' REVIEW-ESCALATION-GREEN >result.txt
