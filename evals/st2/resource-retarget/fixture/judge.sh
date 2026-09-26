#!/usr/bin/env bash
set -euo pipefail

diff -u <(cut -f2 "$CATALOG/events.tsv") <(printf 'grant\nrevoke\ngrant\nidle\n')
test "$(st2 resource ls local.worker --catalog "$CATALOG/net" --host local --json | jq 'length')" -eq 0
