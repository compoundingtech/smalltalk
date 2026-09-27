#!/usr/bin/env bash
set -euo pipefail

requester=person/eval-requester
if [ -s controller-state.json ]; then
  while IFS= read -r agent; do
    st3 agents stop "$agent" --as "$requester" >/dev/null 2>&1 || true
  done < <(jq -r '.imports[]?.agent' controller-state.json)
fi

raw_root="$PWD/raw-pty"
if [ -d "$raw_root" ]; then
  while IFS= read -r id; do
    env -u PTY_SESSION PTY_ROOT="$raw_root" pty kill "$id" >/dev/null 2>&1 || true
    env -u PTY_SESSION PTY_ROOT="$raw_root" pty rm "$id" >/dev/null 2>&1 || true
  done < <(env -u PTY_SESSION PTY_ROOT="$raw_root" pty list --json 2>/dev/null | jq -r '.[].name' || true)
fi

echo "native import eval cleanup complete"
