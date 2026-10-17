#!/usr/bin/env bash
# End-to-end demo of the git-queue prototype on local clones. About ten seconds, no network.
#   tests/git-queue/demo.sh
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
gq() { local m=$1; shift; python3 "$here/gq" --repo "$work/$m" --who "$m" "$@"; }
say() { printf '\n\033[1m== %s\033[0m\n' "$*"; }

work=$(mktemp -d); trap 'rm -rf "$work"' EXIT
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null
export GIT_AUTHOR_NAME=dev GIT_AUTHOR_EMAIL=dev@example.com GIT_COMMITTER_NAME=dev GIT_COMMITTER_EMAIL=dev@example.com

say "A scratch bare repository stands in for GitHub; three clones stand in for three machines"
git init -q --bare -b main "$work/origin.git"
for m in nathan johannes queue1 queue2; do git clone -q "$work/origin.git" "$work/$m" 2>/dev/null; done
git -C "$work/nathan" commit -q --allow-empty -m "initial"; git -C "$work/nathan" push -q origin main
for m in johannes queue1 queue2; do git -C "$work/$m" pull -q origin main; done

say "Nathan and Johannes each publish a change in flight (branches stay private; bundles are refs)"
cd "$work/nathan"; echo nathan > nathan.txt; git add .; git commit -qm "nathan: add nathan.txt"
gq nathan publish HEAD --publisher nathan
cd "$work/johannes"; echo johannes > johannes.txt; git add .; git commit -qm "johannes: add johannes.txt"
base=$(git rev-parse HEAD~1)
gq johannes publish HEAD "$base" --form range --publisher johannes

say "What GitHub-side git actually stores: one ref per bundle, one of them pointing at a blob"
git -C "$work/origin.git" for-each-ref --format='%(objecttype) %(refname)' refs/queue

say "Any machine can list the ready bundles"
gq queue1 ls

say "Two queue machines race for the same bundle: exactly one wins"
tree=$(git -C "$work/nathan" rev-parse HEAD^{tree})
( gq queue1 land "$tree" > "$work/r1" 2>&1; echo "exit $?" >> "$work/r1" ) &
( gq queue2 land "$tree" > "$work/r2" 2>&1; echo "exit $?" >> "$work/r2" ) &
wait
echo "queue1: $(tr '\n' ' ' < "$work/r1")"
echo "queue2: $(tr '\n' ' ' < "$work/r2")"

say "queue2 keeps pulling and lands whatever is left (Johannes's change is replayed onto the new main)"
gq queue2 run --once

say "main now"
git -C "$work/origin.git" log --oneline --graph main
git -C "$work/origin.git" ls-tree --name-only main
say "The queue's refs now: only tombstones remain"
git -C "$work/origin.git" for-each-ref --format='%(objecttype) %(refname)' refs/queue
