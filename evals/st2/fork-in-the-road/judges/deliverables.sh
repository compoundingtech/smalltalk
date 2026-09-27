#!/usr/bin/env bash
# JUDGE: deliverables — >=2 non-empty PROPOSAL.md committed across a/b/c AND a non-empty RECOMMENDATION.md in sup.
set -uo pipefail
ROOT="${CATALOG:-$PWD}"; fail=0; nprop=0
for role in a b c; do
  d="$ROOT/$role"
  git -C "$d" cat-file -e HEAD:PROPOSAL.md 2>/dev/null && [ -s "$d/PROPOSAL.md" ] && nprop=$((nprop+1))
done
[ "$nprop" -ge 2 ] && echo "PASS: $nprop proposers committed a non-empty PROPOSAL.md" || { echo "FAIL: only $nprop committed PROPOSAL.md (need >=2)"; fail=1; }
if git -C "$ROOT/sup" cat-file -e HEAD:RECOMMENDATION.md 2>/dev/null && [ -s "$ROOT/sup/RECOMMENDATION.md" ]; then echo "PASS: sup committed a non-empty RECOMMENDATION.md"; else echo "FAIL: no committed RECOMMENDATION.md in sup/"; fail=1; fi
exit "$fail"
