#!/usr/bin/env bash
set -euo pipefail

root="${CATALOG:-$PWD}"
ledger="$root/worker"
base="$(git -C "$ledger" rev-list --max-parents=0 HEAD)"

for item in 1 2 3 4; do
  progress="$(grep -cE "^done: item-$item( |$)" "$ledger/PROGRESS.md" || true)"
  commits="$(git -C "$ledger" log --format='%s' "$base..HEAD" | grep -cx "feat: item $item" || true)"
  test "$progress" -eq 1
  test "$commits" -eq 1
done

echo "PASS: each stable item has one progress record and one commit"
