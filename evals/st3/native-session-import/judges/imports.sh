#!/usr/bin/env bash
set -euo pipefail

: "${ST_MISSION_RUN:?ST_MISSION_RUN must identify the eval mission run}"
test -s controller-state.json

jq -e '
  .result == "passed"
  and (.imports | length) == 5
  and ([.imports[].driver] | sort) == (["claude", "codex", "omp", "opencode", "pi"] | sort)
  and ([.imports[].session] | unique | length) == 5
  and ([.imports[].agent] | unique | length) == 5
  and all(.imports[];
    .raw_process_stopped == true
    and .resumed_native_id == true
    and .owner_run_id == null
    and (.incarnation | length) > 0
  )
' controller-state.json >/dev/null

while IFS=$'\t' read -r driver agent raw_pid; do
  ! kill -0 "$raw_pid" 2>/dev/null
  st3 agents show "$agent" --all --json \
    | jq -e --arg driver "$driver" '
        .value.state == "running"
        and .value.reachability == "reachable"
        and .value.driver == $driver
        and .value.owner_run_id == null
      ' >/dev/null
done < <(jq -r '.imports[] | [.driver, .agent, (.raw_pid | tostring)] | @tsv' controller-state.json)

echo "PASS: Codex, Claude, Pi, OMP, and OpenCode each crossed an exact raw-process fence into one ownerless durable seat"
