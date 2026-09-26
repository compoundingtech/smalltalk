#!/usr/bin/env bash
set -euo pipefail

diff -u <(cut -f2 events.tsv) <(printf 'grant\nrevoke\ngrant\nidle\n')
while read -r subject; do
  st3 resource read "$subject" --json | jq -e '.actual.status == "removed"' >/dev/null
done < <(cut -f3 events.tsv | sort -u)
