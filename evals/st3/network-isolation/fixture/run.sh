#!/usr/bin/env bash
set -euo pipefail

declare -a daemons=()
root="$(mktemp -d "${TMPDIR:-/tmp}/s3ni.XXXXXX")"
wait_for_terminal_cleanup() {
  local socket="$1" run_id="$2" phase=""
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
  local pty_root="$1" session=""
  while IFS= read -r session; do
    PTY_ROOT="$pty_root" pty kill "$session" >/dev/null 2>&1 || true
    PTY_ROOT="$pty_root" pty rm "$session" >/dev/null 2>&1 || true
  done < <(PTY_ROOT="$pty_root" pty list --json 2>/dev/null | jq -r '.[].name')
}
cleanup() {
  local failed=0
  for name in a b; do
    socket="$root/$name/st3.sock"
    if [ -S "$socket" ] && [ -s "$name/run-id" ]; then
      run_id=$(cat "$name/run-id")
      printf 'version 2\nmission-run "%s" { cancellation "fixture-complete" { reason "the fixture completed" } }\n' "$run_id" >"$name/stop.kdl"
      if st3 --endpoint "$socket" publish "$name/stop.kdl" --as person/fixture >/dev/null 2>&1; then
        wait_for_terminal_cleanup "$socket" "$run_id" || failed=1
      else
        failed=1
      fi
    fi
    remove_test_ptys "$root/$name/pty"
  done
  for daemon in "${daemons[@]:-}"; do kill -TERM "$daemon" 2>/dev/null || true; wait "$daemon" 2>/dev/null || true; done
  rm -rf "$root"
  return "$failed"
}
trap cleanup EXIT
for name in a b; do
  mkdir -p "$name"
  mkdir -p "$root/$name"
  socket="$root/$name/st3.sock"
  state="$root/$name/state"
  st3 up --node "net-$name" --state-dir "$state" --socket "$socket" --pty-root "$root/$name/pty" >"$name/daemon.log" 2>&1 &
  daemons+=("$!")
  for _ in $(seq 1 100); do st3 --endpoint "$socket" doctor >/dev/null 2>&1 && break; sleep 0.05; done
  cat >"$name/network.kdl" <<KDL
version 2
mission "fixture/network-$name" state="ready" {
  goal "Keep one isolated message target and PTY available."
  agent "net.$name" {
    workspace "$PWD"
    command "sleep 300"
    restart "never"
    env { ST3_MESSAGE_ROOT "$state/messages" }
    exec "ding" { argv "st3" "driver" "ding" }
  }
  pty "session-$name" { workspace "$PWD"; command "bash -c 'echo ${name^^}-READY; sleep 300'"; restart "never" }
}
KDL
  st3 --endpoint "$socket" publish "$name/network.kdl" --as person/fixture >/dev/null
  st3 --endpoint "$socket" --json mission start "fixture/network-$name" --workspace "$PWD" --as person/fixture | jq -er .mission_run.id >"$name/run-id"
done
sleep 1
run_a=$(cat a/run-id)
run_b=$(cat b/run-id)
agent_a="agent/$run_a/net.a"
agent_b="agent/$run_b/net.b"
pty_a="pty/$run_a/session-a"
pty_b="pty/$run_b/session-b"
ida="$(st3 --endpoint "$root/a/st3.sock" message send "$agent_a" --from person/source -m NETA-SECRET)"
idb="$(st3 --endpoint "$root/b/st3.sock" message send "$agent_b" --from person/source -m NETB-SECRET)"
st3 --endpoint "$root/a/st3.sock" pty ls --json >a/pty.json
st3 --endpoint "$root/b/st3.sock" pty ls --json >b/pty.json
st3 --endpoint "$root/a/st3.sock" message ls "$agent_a" --json >a/messages.json
st3 --endpoint "$root/b/st3.sock" message ls "$agent_b" --json >b/messages.json
jq -e --arg own "$pty_a" --arg other "$pty_b" 'map(.subject) | index($own) != null and index($other) == null' a/pty.json >/dev/null
jq -e --arg own "$pty_b" --arg other "$pty_a" 'map(.subject) | index($own) != null and index($other) == null' b/pty.json >/dev/null
jq -e 'map(.content) | index("NETA-SECRET") != null and index("NETB-SECRET") == null' a/messages.json >/dev/null
jq -e 'map(.content) | index("NETB-SECRET") != null and index("NETA-SECRET") == null' b/messages.json >/dev/null
printf 'NETWORK-A-ISOLATED\nNETWORK-B-ISOLATED\n' >result.txt
cleanup
trap - EXIT
