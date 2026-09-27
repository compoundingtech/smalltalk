#!/usr/bin/env bash
set -euo pipefail

root="${CATALOG:-$PWD}"
ledger="$root/worker"
supervisor="$root/sup"
test -d "$ledger/.git"
test ! -d "$supervisor/.git"

base="$(git -C "$ledger" rev-list --max-parents=0 HEAD)"
test "$(git -C "$ledger" rev-list --count "$base..HEAD")" -eq 4

unexpected_authors="$(git -C "$ledger" log --format='%ae' "$base..HEAD" | grep -vx 'rc.dev@eval.local' || true)"
test -z "$unexpected_authors"

unexpected_paths="$(git -C "$ledger" diff --name-only "$base..HEAD" | grep -vxE 'src/dispatch[.]js|PROGRESS[.]md' || true)"
test -z "$unexpected_paths"

echo "PASS: rc.dev made four confined commits, and the supervisor owns no repository"
