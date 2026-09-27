#!/usr/bin/env bash
# Shared MUTANT BATTERY for the test-writing eval. Each mutant is a targeted, NON-EQUIVALENT change to
# src/grades.js that a THOROUGH suite must catch but a shallow "one happy path" suite (or coverage
# theater) will miss. Format: id|description|FROM|TO  (FROM is matched LITERALLY; must be unique in the file.)
# Sourced by setup-sandbox.sh (validate a reference suite kills all) and grade.sh (score the team's suite).

MUTANTS=(
"M1|letter A boundary (>=90 -> >90)|score >= 90|score > 90"
"M2|letter B boundary (>=80 -> >80)|score >= 80|score > 80"
"M3|letter C boundary (>=70 -> >70)|score >= 70|score > 70"
"M4|letter D boundary (>=60 -> >60)|score >= 60|score > 60"
"M5|upper-range boundary (>100 -> >=100)|score > 100|score >= 100"
"M6|lower-range boundary (<0 -> <=0)|score < 0|score <= 0"
"M7|gpaPoints A (4 -> 3)|A: 4|A: 3"
"M8|gpaPoints D (1 -> 2)|D: 1|D: 2"
"M9|summary counts (+=1 -> +=2)|counts[l] += 1|counts[l] += 2"
"M10|summary gpa reduce (+ -> -)|map(gpaPoints).reduce((a, b) => a + b, 0)|map(gpaPoints).reduce((a, b) => a - b, 0)"
"M11|summary average (drop the divide)|const average = scores.reduce((a, b) => a + b, 0) / scores.length;|const average = scores.reduce((a, b) => a + b, 0);"
"M12|summary empty guard (===0 -> ===1)|scores.length === 0|scores.length === 1"
)

# run_mutation_score <worker_repo>
# Applies each mutant to a scratch copy of the repo's src/grades.js, runs the repo's tests, and reports
# killed (tests went RED) vs survived (tests stayed GREEN = a coverage gap). Sets MUT_KILLED/MUT_TOTAL/MUT_SURVIVORS.
run_mutation_score() {
  local W="$1"; local killed=0 total=0 survivors=""
  for m in "${MUTANTS[@]}"; do
    IFS='|' read -r id desc from to <<< "$m"
    total=$((total+1))
    local tmp; tmp=$(mktemp -d); cp -R "$W/." "$tmp/" 2>/dev/null; rm -rf "$tmp/.git" "$tmp/node_modules"
    FROM="$from" TO="$to" perl -0777 -pi -e 's/\Q$ENV{FROM}\E/$ENV{TO}/' "$tmp/src/grades.js"
    if cmp -s "$W/src/grades.js" "$tmp/src/grades.js"; then
      echo "  [SKIP] $id ($desc): pattern did not apply — fixture drift or src changed"; survivors="$survivors $id(skip)"; rm -rf "$tmp"; continue
    fi
    if ( cd "$tmp" && node --test >/dev/null 2>&1 ); then
      echo "  [SURVIVED] $id — $desc"; survivors="$survivors $id"
    else
      echo "  [killed]   $id — $desc"; killed=$((killed+1))
    fi
    rm -rf "$tmp"
  done
  MUT_KILLED=$killed; MUT_TOTAL=$total; MUT_SURVIVORS="$survivors"
  echo "MUTATION SCORE: $killed/$total killed"
  [ -n "$survivors" ] && echo "SURVIVORS:$survivors"
  return 0   # never leak a nonzero from the trailing test (would abort a caller running under `set -e`)
}
