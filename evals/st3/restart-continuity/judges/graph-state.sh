#!/usr/bin/env bash
set -euo pipefail

: "${ST_MISSION_RUN:?ST_MISSION_RUN must identify the judged mission run}"
run="mission-run/$ST_MISSION_RUN"
mission="$(env -u ST_AGENT st3 --json mission show "$run")"

completed_steps=(
  process-before-restart
  process-before-restart/work/inspect-durable-state
  process-before-restart/work/process-item-1
  process-before-restart/work/process-item-2
  process-before-restart/work/publish-pre-restart-revision
  process-after-restart
  process-after-restart/work/inspect-recovered-state
  process-after-restart/work/process-item-3
  process-after-restart/work/process-item-4
  process-after-restart/work/verify-complete-batch
  process-after-restart/work/publish-batch-revision
  process-after-restart/work/report-to-supervisor
  verify-and-confirm
  verify-and-confirm/work/inspect-graph-history
  verify-and-confirm/work/verify-ledger-read-only
  verify-and-confirm/work/confirm-requester
)

for step in "${completed_steps[@]}"; do
  count="$(jq \
    --arg run "$run" \
    --arg step "$step" \
    '[.steps[] | select(.step == $step and .status == "completed")] | length' \
    <<<"$mission")"
  test "$count" -eq 1
done

while read -r name kind state_name; do
  subject="resource/mission-run/$ST_MISSION_RUN/$name"
  status="$(st3 subject show "$subject" --json)"
  jq -e --arg kind "$kind" --arg state "$state_name" \
    '.status.subjects[0].actual | (.fields // .)
      | (.kind == $kind) and (.state == $state)' \
    <<<"$status" >/dev/null
  bindings="$(st3 trace "$subject" --json --limit 20 \
    | jq -s '[.[] | select(.kind == "resource.observed")] | length')"
  test "$bindings" -eq 1
done <<'PRODUCTS'
pre-restart vcs.commit published
restart custom.st3.cold-restart injected
batch vcs.commit published
worker-report custom.st3.message-receipt published
verification custom.st3.message-receipt published
PRODUCTS

echo "PASS: the graph records every work step and one binding for each required product"
