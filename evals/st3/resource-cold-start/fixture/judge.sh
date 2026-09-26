#!/usr/bin/env bash
set -euo pipefail

test "$(wc -l <events.tsv)" -eq 1
resource="$(awk -F '\t' 'NR == 1 { print $3 }' events.tsv)"
message="message/resource-cold-start-${ST_MISSION_RUN}"
st3 resource read "$resource" --json | jq -e '.actual.status == "active" and .actual.url == "work://eval/cold-start"' >/dev/null
resource_index="$(st3 subject show "$resource" --json | jq '[.recent_claims[] | select(.kind == "resource.observed")][0].store_index')"
message_index="$(st3 subject show "$message" --json | jq '[.recent_claims[] | select(.kind == "message.delivered")][0].store_index')"
test "$resource_index" -lt "$message_index"
test "$(st3 conversations ls "agent/${ST_MISSION_RUN}/worker" --json | jq --arg message "$message" '[.[] | select(.subject == $message and .status == "delivered" and .content == "work://eval/cold-start is ready")] | length')" -eq 1
