#!/usr/bin/env bash
set -Eeuo pipefail

: "${ST_MISSION_RUN:?ST_MISSION_RUN must identify the eval mission run}"

readonly REQUESTER=person/eval-requester
readonly RAW_PTY_ROOT="$PWD/raw-pty"
readonly STATE="$PWD/controller-state.json"
readonly WORKSPACE="$PWD/native-workspace"
readonly PROMPT="Reply with exactly RAW-IMPORT-READY and do not use tools."
readonly CODEX_BIN="${NATIVE_IMPORT_CODEX_BIN:-codex}"
readonly CLAUDE_BIN="${NATIVE_IMPORT_CLAUDE_BIN:-claude}"
readonly PI_BIN="${NATIVE_IMPORT_PI_BIN:-pi}"
readonly OMP_BIN="${NATIVE_IMPORT_OMP_BIN:-omp}"
readonly OPENCODE_BIN="${NATIVE_IMPORT_OPENCODE_BIN:-opencode}"

mkdir -p "$RAW_PTY_ROOT" "$WORKSPACE"
state='{"result":"running","imports":[]}'
persist_state() { printf '%s\n' "$state" >"$STATE"; }
persist_state

raw_pty() { env -u PTY_SESSION PTY_ROOT="$RAW_PTY_ROOT" pty "$@"; }

cleanup_on_error() {
  local status=$?
  trap - EXIT HUP INT TERM
  if (( status != 0 )); then
    bash ./cleanup.sh >/dev/null 2>&1 || true
  fi
  exit "$status"
}
trap cleanup_on_error EXIT HUP INT TERM

wait_until() {
  local label=$1 seconds=$2
  shift 2
  local deadline=$((SECONDS + seconds))
  while (( SECONDS < deadline )); do
    if "$@"; then return 0; fi
    sleep 1
  done
  printf '%s did not complete within %ss\n' "$label" "$seconds" >&2
  return 1
}

import_items() {
  st3 import ls --all --limit 200 --json 2>/dev/null | jq -c '.value.items'
}

new_saved_session() {
  local driver=$1 before=$2
  import_items | jq -er --arg driver "$driver" --arg workspace "$WORKSPACE" --slurpfile before "$before" '
    map(select(
      .driver == $driver
      and .workspace == $workspace
      and .native_session_id != null
      and (.id as $id | [ $before[0][] ] | index($id) | not)
    ))
    | sort_by(.updated_at)
    | last
    | .id
  ' >/dev/null 2>&1
}

select_new_saved_session() {
  local driver=$1 before=$2
  import_items | jq -er --arg driver "$driver" --arg workspace "$WORKSPACE" --slurpfile before "$before" '
    map(select(
      .driver == $driver
      and .workspace == $workspace
      and .native_session_id != null
      and (.id as $id | [ $before[0][] ] | index($id) | not)
    ))
    | sort_by(.updated_at)
    | last
  '
}

exact_running_session() {
  local session_id=$1
  st3 import show "$session_id" --json 2>/dev/null \
    | jq -e '
        .value.state == "running"
        and .value.importable == true
        and .value.process.exact_session == true
      ' >/dev/null
}

launch_fresh() {
  local driver=$1 pty_id=$2
  case "$driver" in
    codex)
      raw_pty run -d --id "$pty_id" --cwd "$WORKSPACE" -- \
        "$CODEX_BIN" --dangerously-bypass-approvals-and-sandbox --dangerously-bypass-hook-trust
      ;;
    claude)
      raw_pty run -d --id "$pty_id" --cwd "$WORKSPACE" -- \
        "$CLAUDE_BIN" --permission-mode bypassPermissions
      ;;
    pi) raw_pty run -d --id "$pty_id" --cwd "$WORKSPACE" -- "$PI_BIN" ;;
    omp) raw_pty run -d --id "$pty_id" --cwd "$WORKSPACE" -- "$OMP_BIN" ;;
    opencode) raw_pty run -d --id "$pty_id" --cwd "$WORKSPACE" -- "$OPENCODE_BIN" ;;
    *) return 2 ;;
  esac
}

session_transcript() {
  local driver=$1 native=$2
  case "$driver" in
    pi)
      rg -l --fixed-strings "$native" "$HOME/.pi/agent/sessions" --glob '*.jsonl' | head -n 1
      ;;
    omp)
      {
        test ! -d "$HOME/.oh-omp/agent/sessions" || \
          rg -l --fixed-strings "$native" "$HOME/.oh-omp/agent/sessions" --glob '*.jsonl'
        test ! -d "$HOME/.omp/agent/sessions" || \
          rg -l --fixed-strings "$native" "$HOME/.omp/agent/sessions" --glob '*.jsonl'
      } | head -n 1
      ;;
    *) return 2 ;;
  esac
}

launch_resume() {
  local driver=$1 pty_id=$2 native=$3 transcript=""
  case "$driver" in
    codex)
      raw_pty run -d --id "$pty_id" --cwd "$WORKSPACE" -- \
        "$CODEX_BIN" resume "$native" --dangerously-bypass-approvals-and-sandbox --dangerously-bypass-hook-trust
      ;;
    claude)
      raw_pty run -d --id "$pty_id" --cwd "$WORKSPACE" -- \
        "$CLAUDE_BIN" --resume "$native" --permission-mode bypassPermissions
      ;;
    pi)
      transcript="$(session_transcript "$driver" "$native")"
      test -n "$transcript"
      raw_pty run -d --id "$pty_id" --cwd "$WORKSPACE" -- "$PI_BIN" --session "$transcript"
      ;;
    omp)
      raw_pty run -d --id "$pty_id" --cwd "$WORKSPACE" -- "$OMP_BIN" "--resume=$native"
      ;;
    opencode)
      raw_pty run -d --id "$pty_id" --cwd "$WORKSPACE" -- "$OPENCODE_BIN" --session "$native"
      ;;
    *) return 2 ;;
  esac
}

agent_reachable() {
  local agent=$1 driver=$2
  st3 agents show "$agent" --all --json 2>/dev/null \
    | jq -e --arg driver "$driver" '
        .value.state == "running"
        and .value.reachability == "reachable"
        and .value.driver == $driver
        and .value.owner_run_id == null
        and .value.incarnation_id != null
      ' >/dev/null
}

managed_process_has_native_id() {
  local driver=$1 native=$2
  ps -axo command= \
    | grep -F "$native" \
    | grep -E "(^|[/ ])(st3 driver $driver|$driver)( |$)" \
    | grep -v '[g]rep' >/dev/null
}

for driver in codex claude pi omp opencode; do
  pty_id="raw-import-$driver-${ST_MISSION_RUN##*/}"
  before="$PWD/before-$driver.json"
  import_items | jq '[.[].id]' >"$before"

  launch_fresh "$driver" "$pty_id"
  raw_pty send "$pty_id" --seq "$PROMPT" --seq key:return
  wait_until "$driver native session file" 300 new_saved_session "$driver" "$before"
  saved="$(select_new_saved_session "$driver" "$before")"
  session_id="$(jq -er .id <<<"$saved")"
  native="$(jq -er .native_session_id <<<"$saved")"

  raw_pty kill "$pty_id" >/dev/null 2>&1 || true
  wait_until "$driver fresh PTY exit" 30 bash -c \
    "! env -u PTY_SESSION PTY_ROOT='$RAW_PTY_ROOT' pty list --json | jq -e --arg id '$pty_id' 'any(.[]; .name == \$id and .status == \"running\")' >/dev/null"
  raw_pty rm "$pty_id" >/dev/null 2>&1 || true

  launch_resume "$driver" "$pty_id" "$native"
  wait_until "$driver exact running discovery" 120 exact_running_session "$session_id"
  live="$(st3 import show "$session_id" --json | jq -er .value)"
  raw_pid="$(jq -er .process.pid <<<"$live")"
  revision="$(jq -er .revision <<<"$live")"

  receipt="$(st3 import run "$session_id" --as "$REQUESTER" --json)"
  agent="$(jq -r '.. | strings | select(startswith("agent/import/"))' <<<"$receipt" | head -n 1)"
  test -n "$agent"
  wait_until "$driver imported seat" 180 agent_reachable "$agent" "$driver"
  wait_until "$driver native resume command" 60 managed_process_has_native_id "$driver" "$native"
  if kill -0 "$raw_pid" 2>/dev/null; then
    printf '%s raw process %s survived fenced import\n' "$driver" "$raw_pid" >&2
    exit 1
  fi
  incarnation="$(st3 agents show "$agent" --all --json | jq -er .value.incarnation_id)"
  state="$(jq \
    --arg driver "$driver" --arg session "$session_id" --arg native "$native" \
    --arg revision "$revision" --arg agent "$agent" --arg incarnation "$incarnation" \
    --argjson raw_pid "$raw_pid" '
      .imports += [{
        driver: $driver, session: $session, native_session_id: $native,
        revision: $revision, raw_pid: $raw_pid, raw_process_stopped: true,
        agent: $agent, incarnation: $incarnation, owner_run_id: null,
        resumed_native_id: true
      }]
    ' <<<"$state")"
  persist_state
done

state="$(jq '.result = "passed"' <<<"$state")"
persist_state
trap - EXIT HUP INT TERM
printf 'all five raw PTY harness sessions imported into durable seats\n'
