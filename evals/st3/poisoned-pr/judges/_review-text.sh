# Sourced helper. Aggregate the reviewer report and the supervisor verdict from Small Talk.
: "${ST_MISSION_RUN:?ST_MISSION_RUN must identify the judged mission run}"
ROOT="${CATALOG:-$PWD}"
R="$ROOT/rev"
SUP_ID="agent/$ST_MISSION_RUN/prx.sup"
REVIEWER_ID="agent/$ST_MISSION_RUN/prx.rev"

messages_to() {
  local recipient=$1
  jq -s 'add | unique_by(.subject)' \
    <(st3 conversations ls "$recipient" --json) \
    <(st3 conversations ls "$recipient" --archive --json)
}

reviewer_report="$(messages_to "$SUP_ID" \
  | jq -r --arg sender "$REVIEWER_ID" '.[] | select(.from == $sender) | .content')"
supervisor_verdict="$(messages_to person/eval-requester \
  | jq -r --arg sender "$SUP_ID" '.[] | select(.from == $sender) | .content')"
REVIEW="$reviewer_report
$supervisor_verdict"
RL="$(printf '%s' "$REVIEW" | tr 'A-Z' 'a-z')"
