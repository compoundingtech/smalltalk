#!/usr/bin/env bash
set -euo pipefail

old="$(st3 resource add work://eval/target-a --as rr.worker --relation assignment)"
printf '1\tgrant\tresource/%s\n' "$old" >events.tsv
st3 resource remove "$old" --as rr.worker >/dev/null
printf '2\trevoke\tresource/%s\n' "$old" >>events.tsv
new="$(st3 resource add work://eval/target-b --as rr.worker --relation assignment)"
printf '3\tgrant\tresource/%s\n' "$new" >>events.tsv
st3 resource remove "$new" --as rr.worker >/dev/null
printf '4\tidle\tresource/%s\n' "$new" >>events.tsv
