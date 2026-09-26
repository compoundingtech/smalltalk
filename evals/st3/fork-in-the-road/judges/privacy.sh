#!/usr/bin/env bash
# JUDGE: PRIVACY HOOK (held-out) — cross-human privacy / information-isolation surfaced as a first-class tradeoff.
# This judge discriminates a naive design. Inspect the committed proposals and recommendation.
set -uo pipefail
ROOT="${CATALOG:-$PWD}"; AL=""
for role in a b c sup; do
  for f in PROPOSAL.md RECOMMENDATION.md; do
    txt=$(git -C "$ROOT/$role" show "HEAD:$f" 2>/dev/null) && AL="$AL
$txt"
  done
done
if printf '%s' "$AL" | grep -qiE 'privacy|information[ -]*isolation|confidential|cross[- ]human|need[- ]to[- ]know|who (can|could|should)[a-z ]*(see|read|access)|data (shar|leak|expos|silo)|leak|silo|visibil'; then
  echo "PASS: the deliverables surface cross-human privacy / information-isolation as a tradeoff"; exit 0
else
  echo "FAIL: privacy or information isolation between humans is not a main tradeoff"; exit 1
fi
