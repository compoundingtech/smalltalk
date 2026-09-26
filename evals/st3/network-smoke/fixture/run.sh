#!/usr/bin/env bash
set -euo pipefail

root="$(mktemp -d "${TMPDIR:-/tmp}/st3-network-smoke.XXXXXX")"
state="$root/state"
socket="$root/st3.sock"
pty_root="$root/p"
daemon=""
run_id=""
wait_for_terminal_cleanup() {
  local phase=""
  for _ in $(seq 1 200); do
    phase="$(st3 --endpoint "$socket" inspect "mission-run/$run_id" --json 2>/dev/null \
      | jq -r '.status.subjects[0].actual.phase // .status.subjects[0].actual.fields.phase // empty')"
    [ "$phase" = terminal ] && return
    sleep 0.05
  done
  printf 'mission-run/%s did not finish runtime cleanup\n' "$run_id" >&2
  return 1
}
remove_test_ptys() {
  local session=""
  while IFS= read -r session; do
    PTY_ROOT="$pty_root" pty kill "$session" >/dev/null 2>&1 || true
    PTY_ROOT="$pty_root" pty rm "$session" >/dev/null 2>&1 || true
  done < <(PTY_ROOT="$pty_root" pty list --json 2>/dev/null | jq -r '.[].name')
}
cleanup() {
  local failed=0
  if [ -S "$socket" ] && [ -n "$run_id" ]; then
    printf 'version 2\nmission-run "%s" { cancellation "fixture-complete" { reason "the fixture completed" } }\n' "$run_id" >stop.kdl
    if st3 --endpoint "$socket" publish stop.kdl --as person/fixture >/dev/null 2>&1; then
      wait_for_terminal_cleanup || failed=1
    else
      failed=1
    fi
  fi
  remove_test_ptys
  if [ -n "$daemon" ]; then kill -TERM "$daemon" 2>/dev/null || true; wait "$daemon" 2>/dev/null || true; fi
  rm -f "$socket"
  rm -rf "$root"
  return "$failed"
}
finish() {
  local status=$?
  trap - EXIT
  if ! cleanup && [ "$status" -eq 0 ]; then
    status=1
  fi
  exit "$status"
}
trap finish EXIT
st3 up --node smoke --state-dir "$state" --socket "$socket" --pty-root "$pty_root" >daemon.log 2>&1 &
daemon=$!
for _ in $(seq 1 100); do st3 --endpoint "$socket" doctor >/dev/null 2>&1 && break; sleep 0.05; done
st3 --endpoint "$socket" doctor >/dev/null
printf 'NETWORK-SMOKE-HEALTH-GREEN\n' >result.txt
cat >network.kdl <<KDL
version 2
mission "fixture/network-smoke" state="ready" {
  goal "Keep one message target available."
  agent "net.dev" {
    workspace "$PWD"
    command "sleep 300"
    restart "on-failure"
    exec "ding" {
      argv "st3" "driver" "ding"
    }
  }
}
KDL
st3 --endpoint "$socket" publish network.kdl --as person/fixture >/dev/null
run_id="$(st3 --endpoint "$socket" --json mission start fixture/network-smoke --workspace "$PWD" --as person/fixture | jq -er .mission_run.id)"
agent="agent/$run_id/net.dev"
for _ in $(seq 1 100); do st3 --endpoint "$socket" agents --json | jq -e --arg agent "$agent" '.[] | select(.subject == $agent and .actual.status == "running")' >/dev/null 2>&1 && break; sleep 0.05; done
st3 --endpoint "$socket" agents --json | jq -e --arg agent "$agent" '.[] | select(.subject == $agent and .actual.status == "running")' >/dev/null
id="$(st3 --endpoint "$socket" message send "$agent" --from person/tester -m NETWORK-SMOKE-ROUNDTRIP)"
for _ in $(seq 1 100); do
  st3 --endpoint "$socket" inspect "message/$id" --json | jq -e '.recent_claims | map(.kind) | index("message.delivered") != null' >/dev/null 2>&1 && break
  sleep 0.05
done
st3 --endpoint "$socket" inspect "message/$id" --json | jq -e '.recent_claims | map(.kind) | index("message.delivered") != null' >/dev/null
printf 'NETWORK-SMOKE-DELIVERY-GREEN\n' >>result.txt
cleanup
trap - EXIT
