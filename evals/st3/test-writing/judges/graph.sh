#!/usr/bin/env bash
set -euo pipefail

: "${ST_MISSION_RUN:?ST_MISSION_RUN must identify the judged mission run}"
run="mission-run/$ST_MISSION_RUN"
mission="$(env -u ST_AGENT st3 --json mission show "$run")"

completed_steps=(
  start-team
  prepare-test-brief
  prepare-test-brief/work/inspect-module-read-only
  prepare-test-brief/work/send-test-brief
  write-regression-suite
  write-regression-suite/work/read-test-brief
  write-regression-suite/work/test-letter-boundaries
  write-regression-suite/work/test-gpa-and-summary
  write-regression-suite/work/run-complete-suite
  write-regression-suite/work/publish-test-revision
  write-regression-suite/work/report-test-result
  verify-test-suite
  verify-test-suite/work/read-developer-report
  verify-test-suite/work/verify-read-only
  verify-test-suite/work/confirm-requester
)

for step in "${completed_steps[@]}"; do
  count="$(jq --arg run "$run" --arg step "$step" \
    '[.steps[] | select(.step == $step and .status == "completed")] | length' \
    <<<"$mission")"
  test "$count" -eq 1
done

while read -r name kind; do
  subject="resource/mission-run/$ST_MISSION_RUN/$name"
  status="$(st3 subject show "$subject" --json)"
  jq -e --arg kind "$kind" '
    .status.subjects[0].actual | (.fields // .)
      | (.kind == $kind) and (.state == "published")' \
    <<<"$status" >/dev/null
  bindings="$(st3 trace "$subject" --json --limit 20 \
    | jq -s '[.[] | select(.kind == "resource.observed")] | length')"
  test "$bindings" -ge 1
done <<'PRODUCTS'
test-brief custom.st3.message-receipt
test-revision vcs.commit
developer-report custom.st3.message-receipt
final-assessment custom.st3.message-receipt
PRODUCTS

published="$(st3 subject show "resource/mission-run/$ST_MISSION_RUN/test-revision" --json \
  | jq -r '.status.subjects[0].actual | (.fields // .) | .sha')"
current="$(git -C "$CATALOG/worker" rev-parse HEAD)"
test "$published" = "$current"

echo "PASS: the graph records the complete Test Writing mission and products"
