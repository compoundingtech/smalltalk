#!/usr/bin/env bash
set -euo pipefail

net="$CATALOG/net"
printf '%s\n' CONTEXT-NOW-7b9d | st2 context write cr.agent --catalog "$net" --host cr --as cr.agent
st2 context append cr.agent --catalog "$net" --host cr --as cr.agent --decision DECISION-7b9d --why DECISION-WHY-7b9d >/dev/null
ref="continuity"
st2 resource add "$ref" --uri https://example.invalid/context-resource-7b9d --reason CONTINUITY-RESOURCE-7b9d --catalog "$net" --host cr --as cr.agent --agent cr.agent >/dev/null
for cycle in 1 2; do
  st2 up --once --catalog "$net" --host cr >/dev/null
  st2 down --catalog "$net" --host cr >/dev/null
  test "$(PTY_ROOT="$net/pty" pty list --json | jq '[.[] | select(.status == "running")] | length')" -eq 0
  printf 'cycle-%s\n' "$cycle"
done
st2 context read cr.agent --catalog "$net" --host cr --full
st2 resource read cr.agent "$ref" --catalog "$net" --host cr
printf 'cleanup-green\n'
