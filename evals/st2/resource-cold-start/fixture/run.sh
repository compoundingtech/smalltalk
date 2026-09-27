#!/usr/bin/env bash
set -euo pipefail

net="$CATALOG/net"
events="$CATALOG/events.tsv"
ref="cold-start"
st2 resource add "$ref" --uri work://eval/cold-start --reason "Cold start work" --catalog "$net" --host local --as local.worker --agent local.worker >/dev/null
st2 resource read local.worker "$ref" --catalog "$net" --host local | grep -Fq work://eval/cold-start
printf '1\tresource-ready\t%s\n' "$ref" >"$events"
message="$(st2 message send local.worker --catalog "$net" --host local --as local.worker --subject "Resource ready" -m "work://eval/cold-start is ready")"
printf '2\tdelivery\t%s\n' "$message" >>"$events"
printf 'RESOURCE-COLD-START-GREEN\n'
