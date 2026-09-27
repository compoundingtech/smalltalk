#!/usr/bin/env bash
# ISOLATION (hard gate): per-author PATH-lane attribution on the integrated workspace. Each agent's commits may
# touch ONLY its package lane — sig-base the base pkg (signal/|beacon/), sig-relay the *-relay/, sig-hub the
# *-hub/, sig-sup config/ + workspace root. Any cross-lane commit (or an unexpected author) FAILS the run.
set -uo pipefail
. "$(dirname "$0")/_integrate.sh"
lane_ok=1
while read -r sha; do
  [ -z "$sha" ] && continue
  ae=$(git -C "$W" show -s --format='%ae' "$sha")
  files=$(git -C "$W" show --name-only --format='' "$sha" | grep -v '^$' || true)
  case "$ae" in
    *sig-base@*)  bad=$(echo "$files" | grep -vE '^(signal|beacon)/'            || true) ;;
    *sig-relay@*) bad=$(echo "$files" | grep -vE '^(signal-relay|beacon-relay)/' || true) ;;
    *sig-hub@*)   bad=$(echo "$files" | grep -vE '^(signal-hub|beacon-hub)/'    || true) ;;
    *sig-sup@*)   bad=$(echo "$files" | grep -vE '^(config/|package\.json|README\.md|\.gitignore)' || true) ;;
    *) echo "  FAIL: commit $sha by UNEXPECTED author $ae (not a pinned lane owner)"; lane_ok=0; bad="" ;;
  esac
  if [ -n "$bad" ]; then echo "  FAIL: ${ae%%@*} changed out-of-lane files in $sha: $(echo "$bad" | tr '\n' ' ')"; lane_ok=0; fi
done < <(git -C "$W" rev-list "$BASE"..HEAD 2>/dev/null)
[ "$lane_ok" -eq 1 ] || { echo "FAIL: cross-lane / unexpected-author commit(s) — isolation broken"; exit 1; }
echo "PASS: every commit stayed in its author's package lane (base/relay/hub/sup) — isolation held"
