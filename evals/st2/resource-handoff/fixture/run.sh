#!/usr/bin/env bash
set -euo pipefail

net="$CATALOG/net"
uri="work://eval/shared-handoff"
a="assignment"
st2 resource add "$a" --uri "$uri" --reason "The resource assigned to A" --catalog "$net" --host local --as local.a --agent local.a >/dev/null
printf '1\ta\tactive\n' >"$CATALOG/events.tsv"
st2 resource remove "$a" --agent local.a --catalog "$net" --host local --as local.a >/dev/null
printf '2\ta\trevoked\n' >>"$CATALOG/events.tsv"
b="assignment"
st2 resource add "$b" --uri "$uri" --reason "The resource handed to B" --catalog "$net" --host local --as local.b --agent local.b >/dev/null
printf '3\tb\tactive\n' >>"$CATALOG/events.tsv"
st2 resource read local.b "$b" --catalog "$net" --host local | grep -Fq "$uri"
