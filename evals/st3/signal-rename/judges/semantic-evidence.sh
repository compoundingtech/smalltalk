#!/usr/bin/env bash
set -euo pipefail

: "${ST_MISSION_RUN:?ST_MISSION_RUN must identify the judged mission run}"

printf '\n### mission run claim\n'
"$ST3_BIN" inspect "mission-run/$ST_MISSION_RUN" --json \
  | jq -c '{status: (.status.subjects[0].actual.fields.status // .status.subjects[0].actual.status)}'

printf '\n### durable work state\n'
env -u ST_AGENT "$ST3_BIN" work ls --all --json \
  | jq -c --arg run "mission-run/$ST_MISSION_RUN" \
      '[.[] | select(.run == $run) | {step, status, assigned_to, available_to, claimant, updated_at_unix_ms}]'

for product in \
  base-compatibility \
  relay-revision \
  hub-revision \
  config-revision \
  base-final-revision \
  integrated-revision
do
  printf '\n### product: %s\n' "$product"
  "$ST3_BIN" inspect "resource/mission-run/$ST_MISSION_RUN/$product" --json \
    | jq -c '[.recent_claims[] | select(.kind == "resource.observed")][0] | {store_index, actor, fields: (.body.fields // .body)}'
done

printf '\n### integrated commits and changed files\n'
. "$(dirname "$0")/_integrate.sh"
git -C "$W" log --reverse \
  --format='commit %H%nAuthor: %an <%ae>%nSubject: %s' \
  --name-only "$BASE"..HEAD

printf '\n### mechanical results\n'
for judge in isolation suite-green rename primitive e2e; do
  bash "$(dirname "$0")/$judge.sh"
done
