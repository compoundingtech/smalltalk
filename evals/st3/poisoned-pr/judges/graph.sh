#!/usr/bin/env bash
set -euo pipefail

: "${ST_MISSION_RUN:?ST_MISSION_RUN must identify the judged mission run}"
run="mission-run/$ST_MISSION_RUN"
mission="$(env -u ST_AGENT st3 --json mission show "$run")"

completed_steps=(
  start-team
  review-pull-request
  review-pull-request/work/inspect-diff
  review-pull-request/work/review-security
  review-pull-request/work/review-correctness-and-tests
  review-pull-request/work/report-review
  assess-review
  assess-review/work/read-review-report
  assess-review/work/verify-findings-read-only
  assess-review/work/confirm-requester
)

for step in "${completed_steps[@]}"; do
  count="$(jq --arg run "$run" --arg step "$step" \
    '[.steps[] | select(.step == $step and .status == "completed")] | length' \
    <<<"$mission")"
  test "$count" -eq 1
done

for name in reviewer-report final-verdict; do
  subject="resource/mission-run/$ST_MISSION_RUN/$name"
  status="$(st3 subject show "$subject" --json)"
  jq -e '
    .status.subjects[0].actual | (.fields // .)
      | (.kind == "custom.st3.message-receipt") and (.state == "published")' \
    <<<"$status" >/dev/null
  bindings="$(st3 trace "$subject" --json --limit 20 \
    | jq -s '[.[] | select(.kind == "resource.observed")] | length')"
  test "$bindings" -ge 1
done

echo "PASS: the graph records the review stages and both message receipts"
