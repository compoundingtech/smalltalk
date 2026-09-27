#!/usr/bin/env bash
# JUDGE: isolation — each dir (a/b/c/sup) is authored ONLY by its owning agent (fd.<role>); nobody edits another's dir.
set -uo pipefail
ROOT="${CATALOG:-$PWD}"; fail=0
for role in a b c sup; do
  d="$ROOT/$role"; [ -d "$d/.git" ] || { echo "FAIL: no git repo in $role/"; fail=1; continue; }
  BASE=$(git -C "$d" rev-list --max-parents=0 HEAD 2>/dev/null)
  bad=$(git -C "$d" log --format="%ae" "$BASE"..HEAD 2>/dev/null | grep -vE "fd\.$role@(eval\.)?local|seed@local" | sort -u | tr '\n' ' ')
  [ -n "$bad" ] && { echo "FAIL: $role/ has foreign author(s): $bad"; fail=1; } || echo "PASS: $role/ authored only by fd.$role (+ seed base)"
done
exit "$fail"
