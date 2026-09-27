#!/usr/bin/env bash
set -euo pipefail

root="${CATALOG:-$PWD}"
ledger="$root/worker"
log="$root/.stev/restart.log"

test -d "$ledger/.git"
test -f "$root/.stev/restart.done"
test -f "$log"
grep -qx 'action=cold_restart' "$log"
grep -qx 'restart_requested=true' "$log"

pre_head="$(sed -n 's/^pre_restart_head=//p' "$log" | tail -1)"
duplicate="$(sed -n 's/^duplicate_message=//p' "$log" | tail -1)"
test -n "$pre_head"
test -n "$duplicate"
git -C "$ledger" cat-file -e "$pre_head^{commit}"

base="$(git -C "$ledger" rev-list --max-parents=0 HEAD)"
git -C "$ledger" merge-base --is-ancestor "$base" "$pre_head"
git -C "$ledger" merge-base --is-ancestor "$pre_head" HEAD

subject_count() {
  git -C "$ledger" log --format='%s' "$1" | grep -cx "feat: item $2" || true
}

test "$(subject_count "$base..$pre_head" 1)" -eq 1
test "$(subject_count "$base..$pre_head" 2)" -eq 1
test "$(subject_count "$base..$pre_head" 3)" -eq 0
test "$(subject_count "$base..$pre_head" 4)" -eq 0
test "$(subject_count "$pre_head..HEAD" 1)" -eq 0
test "$(subject_count "$pre_head..HEAD" 2)" -eq 0
test "$(subject_count "$pre_head..HEAD" 3)" -eq 1
test "$(subject_count "$pre_head..HEAD" 4)" -eq 1

echo "PASS: items 1 and 2 precede the cold restart, and items 3 and 4 follow it"
