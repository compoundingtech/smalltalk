#!/usr/bin/env bash
set -euo pipefail

for marker in CONTEXT-NOW-7b9d DECISION-WHY-7b9d CONTINUITY-RESOURCE-7b9d cycle-1 cycle-2 state-green cleanup-green; do
  grep -Fq "$marker" result.txt
done
test -s resource-ref
