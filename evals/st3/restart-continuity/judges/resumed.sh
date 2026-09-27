#!/usr/bin/env bash
set -euo pipefail

: "${ST_MISSION_RUN:?ST_MISSION_RUN must identify the judged mission run}"
root="${CATALOG:-$PWD}"
ledger="$root/worker"
log="$root/.stev/restart.log"

test -d "$ledger/.git"
test -f "$root/.stev/restart.done"
test -f "$log"
grep -qx 'action=cold_restart' "$log"

pre_head="$(sed -n 's/^pre_restart_head=//p' "$log" | tail -1)"
old_incarnation="$(sed -n 's/^old_incarnation=//p' "$log" | tail -1)"
new_incarnation="$(sed -n 's/^new_incarnation=//p' "$log" | tail -1)"
duplicate="$(sed -n 's/^duplicate_message=//p' "$log" | tail -1)"
test -n "$pre_head"
test -n "$old_incarnation"
test -n "$new_incarnation"
test "$old_incarnation" != "$new_incarnation"
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

restart="$(st3 subject show "resource/mission-run/$ST_MISSION_RUN/restart" --json)"
jq -e \
  --arg old "$old_incarnation" \
  --arg new "$new_incarnation" \
  --arg duplicate "$duplicate" \
  '.status.subjects[0].actual | (.fields // .)
    | .kind == "custom.st3.cold-restart"
    and .state == "injected"
    and .old_incarnation == $old
    and .new_incarnation == $new
    and .duplicate_message == $duplicate' \
  <<<"$restart" >/dev/null

echo "PASS: two worker incarnations split the ordered item commits at the recorded revision"
