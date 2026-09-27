#!/usr/bin/env bash
# JUDGE: distinct — the committed PROPOSAL.md files are not near-duplicates (>=2 distinct normalized texts).
set -uo pipefail
ROOT="${CATALOG:-$PWD}"; tmp=$(mktemp -d); n=0
for role in a b c; do
  f="$ROOT/$role/PROPOSAL.md"; [ -s "$f" ] || continue
  tr '[:upper:]' '[:lower:]' < "$f" | tr -cd 'a-z0-9' | md5sum | cut -d' ' -f1 >> "$tmp/h"; n=$((n+1))
done
uniq_n=$(sort -u "$tmp/h" 2>/dev/null | grep -c .); rm -rf "$tmp"
[ "${uniq_n:-0}" -ge 2 ] && { echo "PASS: $uniq_n distinct proposal texts (a real option space, of $n)"; exit 0; } || { echo "FAIL: proposals are duplicates ($uniq_n distinct of $n)"; exit 1; }
