#!/usr/bin/env bash
# JUDGE (the discriminator): MUTATION SCORE — how many of 12 planted mutants (boundary + aggregation
# changes to src/grades.js) the team's suite KILLS. Coverage theater survives the boundary/aggregation
# mutants; a thorough suite kills them. PASS iff killed >= 10/12 (12 = perfect; 10-11 = strong-with-gaps;
# < 10 = coverage theater = FAIL). run_mutation_score is sourced from judges/mutants.sh (grader-only).
set -uo pipefail
ROOT="${CATALOG:-$PWD}"; W="$ROOT/worker"
[ -d "$W" ] || { echo "FAIL: no worker repo at $W"; exit 1; }
. "$(dirname "$0")/mutants.sh"
MUT_OUT="$(run_mutation_score "$W")"
printf '%s\n' "$MUT_OUT" | sed 's/^/  /'
killed=$(printf '%s\n' "$MUT_OUT" | sed -n 's/.*MUTATION SCORE: \([0-9][0-9]*\)\/\([0-9][0-9]*\).*/\1/p')
total=$(printf '%s\n'  "$MUT_OUT" | sed -n 's/.*MUTATION SCORE: \([0-9][0-9]*\)\/\([0-9][0-9]*\).*/\2/p')
survivors=$(printf '%s\n' "$MUT_OUT" | sed -n 's/^SURVIVORS://p')
killed=${killed:-0}; total=${total:-0}
if [ "$total" -gt 0 ] && [ "$killed" = "$total" ]; then
  echo "PASS: MUTATION SCORE $killed/$total — the suite kills every planted mutant (no coverage gaps)"; exit 0
elif [ "$total" -gt 0 ] && [ "$killed" -ge $(( total * 10 / 12 )) ]; then
  echo "PASS: MUTATION SCORE $killed/$total — strong (kills >= 10/12); survivors:$survivors"; exit 0
else
  echo "FAIL: MUTATION SCORE $killed/$total — coverage theater / shallow suite; survivors:$survivors"; exit 1
fi
