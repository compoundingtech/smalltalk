#!/usr/bin/env bash
set -euo pipefail

net="$CATALOG/net"
events="$CATALOG/events.tsv"
old="assignment"
st2 resource add "$old" --uri work://eval/target-a --reason "The current assignment target" --catalog "$net" --host local --as local.worker --agent local.worker >/dev/null
printf '1\tgrant\twork://eval/target-a\n' >"$events"
st2 resource remove "$old" --agent local.worker --catalog "$net" --host local --as local.worker >/dev/null
printf '2\trevoke\twork://eval/target-a\n' >>"$events"
new="assignment"
st2 resource add "$new" --uri work://eval/target-b --reason "The retargeted assignment" --catalog "$net" --host local --as local.worker --agent local.worker >/dev/null
printf '3\tgrant\twork://eval/target-b\n' >>"$events"
st2 resource read local.worker "$new" --catalog "$net" --host local | grep -Fq work://eval/target-b
st2 resource remove "$new" --agent local.worker --catalog "$net" --host local --as local.worker >/dev/null
printf '4\tidle\twork://eval/target-b\n' >>"$events"
