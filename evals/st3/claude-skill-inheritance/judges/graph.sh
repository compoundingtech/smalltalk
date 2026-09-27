#!/usr/bin/env bash
set -euo pipefail

: "${ST_MISSION_RUN:?ST_MISSION_RUN must identify the judged mission run}"
run="mission-run/$ST_MISSION_RUN"
mission="$(env -u ST_AGENT st3 --json mission show "$run")"

completed_steps=(
  start-worker
  exercise-skill-union
  exercise-skill-union/work/discover-eval-skills
  exercise-skill-union/work/invoke-project-skill
  exercise-skill-union/work/invoke-plugin-skill
  exercise-skill-union/work/verify-skill-effects
  exercise-skill-union/work/report-skill-check
)

for step in "${completed_steps[@]}"; do
  count="$(jq --arg run "$run" --arg step "$step" \
    '[.steps[] | select(.step == $step and .status == "completed")] | length' \
    <<<"$mission")"
  test "$count" -eq 1
done

subject="resource/mission-run/$ST_MISSION_RUN/skill-report"
status="$(st3 subject show "$subject" --json)"
jq -e '
  .status.subjects[0].actual | (.fields // .)
    | (.kind == "custom.st3.message-receipt") and (.state == "published")' \
  <<<"$status" >/dev/null
bindings="$(st3 trace "$subject" --json --limit 20 \
  | jq -s '[.[] | select(.kind == "resource.observed")] | length')"
test "$bindings" -ge 1

echo "PASS: the graph records the complete Claude Skill Inheritance mission and report"
