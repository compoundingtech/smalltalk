#!/usr/bin/env bash
set -euo pipefail

events="$PWD/events.tsv"
ref="$(st3 resource add work://eval/cold-start --as rc.worker --title "Cold start work" --tag work,ready --relation assignment)"
st3 resource read "$ref" --as rc.worker --json | jq -e '.actual.status == "active" and .actual.owner == "agent/rc.worker"' >/dev/null
printf '1\tresource-ready\tresource/%s\n' "$ref" >"$events"
printf 'RESOURCE-COLD-START-GREEN\n'
