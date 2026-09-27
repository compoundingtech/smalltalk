#!/usr/bin/env bash
set -euo pipefail

test "$(wc -l <"$CATALOG/events.tsv")" -eq 2
sed -n '1p' "$CATALOG/events.tsv" | grep -Fq $'1\tresource-ready'
sed -n '2p' "$CATALOG/events.tsv" | grep -Fq $'2\tdelivery'
st2 resource read local.worker cold-start --catalog "$CATALOG/net" --host local | grep -Fq work://eval/cold-start
test "$(grep -rl 'work://eval/cold-start is ready' "$CATALOG/net/local/worker/resources/inbox" | wc -l)" -eq 1
grep -Fq RESOURCE-COLD-START-GREEN "$RUNS_DIR/contract.out"
