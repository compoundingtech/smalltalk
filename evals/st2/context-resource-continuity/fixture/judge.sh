#!/usr/bin/env bash
set -euo pipefail

for marker in CONTEXT-NOW-7b9d DECISION-WHY-7b9d CONTINUITY-RESOURCE-7b9d cycle-1 cycle-2 cleanup-green; do
  grep -Fq "$marker" "$RUNS_DIR/contract.out"
done
