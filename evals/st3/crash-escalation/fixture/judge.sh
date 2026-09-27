#!/usr/bin/env bash
set -euo pipefail

crash="exec/$ST_MISSION_RUN/eval/crash-escalation/crash"
clean="exec/$ST_MISSION_RUN/eval/crash-escalation/clean"
st3 trace "$crash" --json --limit 30 | jq -s -e '[.[] | select(.kind == "runtime.reconcile-decision") | .body.fields.decision] | index("raise") != null' >/dev/null
st3 trace "$clean" --json --limit 30 | jq -s -e '[.[] | select(.kind == "runtime.reconcile-decision") | .body.fields.decision] | index("raise") == null' >/dev/null
