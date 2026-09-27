#!/usr/bin/env bash
set -Eeuo pipefail

root="$(mktemp -d "${TMPDIR:-/tmp}/st3-readiness-recovery.XXXXXX")"
state="$root/state"
socket="$root/st3.sock"
pty_root="/tmp/st3-rr.$$.$RANDOM"
mkdir -p "$pty_root"
daemon=""
run_id=""
agent=""
failure_line=""

trap 'failure_line=$LINENO' ERR

cleanup() {
  local outcome=$?
  if (( outcome != 0 )); then
    echo "runtime readiness recovery failed near line ${failure_line:-unknown}; isolated diagnostics follow" >&2
    if [[ -n "$agent" && -S "$socket" ]]; then
      st3 --endpoint "$socket" status "$agent" --json >&2 || true
      st3 --endpoint "$socket" inspect "$agent" --json >&2 || true
      st3 --endpoint "$socket" attention ls --as person/operator --json >&2 || true
      st3 --endpoint "$socket" message ls "$agent" --archive --json >&2 || true
    fi
    sed -n '1,240p' daemon.log >&2 || true
  fi
  if [[ -n "$run_id" && -S "$socket" ]]; then
    printf 'version 2\nmission-run "%s" { cancellation "eval-finished" { reason "the isolated eval finished" } }\n' "$run_id" \
      | st3 --endpoint "$socket" publish - --as person/eval >/dev/null 2>&1 || true
  fi
  while IFS= read -r session; do
    PTY_ROOT="$pty_root" pty kill "$session" >/dev/null 2>&1 || true
    PTY_ROOT="$pty_root" pty rm "$session" >/dev/null 2>&1 || true
  done < <(PTY_ROOT="$pty_root" pty list --json 2>/dev/null | jq -r '.[].name')
  if [[ -n "$daemon" ]]; then
    kill -TERM "$daemon" >/dev/null 2>&1 || true
    wait "$daemon" >/dev/null 2>&1 || true
  fi
  rm -rf "$pty_root"
  rm -rf "$root"
}
trap cleanup EXIT HUP INT TERM

await_jq() {
  local timeout_seconds=$1
  local filter=$2
  shift 2
  local deadline=$((SECONDS + timeout_seconds))
  while (( SECONDS < deadline )); do
    if "$@" 2>/dev/null | jq -e "$filter" >/dev/null; then
      return 0
    fi
    sleep 0.1
  done
  "$@" | jq -e "$filter" >/dev/null
}

PATH="$PWD/bin:$PATH" st3 up --node readiness --state-dir "$state" --socket "$socket" --pty-root "$pty_root" \
  >daemon.log 2>&1 &
daemon=$!
await_jq 10 '.checks | all(.status == "pass" or .status == "warn")' \
  st3 --endpoint "$socket" doctor --json

cat >fixture.kdl <<KDL
version 2

mission "fixture/readiness-recovery" state="ready" {
  goal "Keep one fake harness available for readiness checks."
  agent "worker" {
    workspace "$PWD"
    harness "codex" {}
    restart "always"
  }
  step "queued" {
    assigned-to "agent/\${ST_MISSION_RUN}/worker"
    goal "Remain ready until the eval claims this work."
  }
}
KDL
st3 --endpoint "$socket" publish fixture.kdl --as person/eval >/dev/null
run_id="$(st3 --endpoint "$socket" --json mission start fixture/readiness-recovery \
  --workspace "$PWD" --as person/eval | jq -er '.mission_run.id')"
agent="agent/$run_id/worker"

await_jq 20 '.subjects[0].actual.status == "running" and (.subjects[0].actual.incarnation_id | length > 0)' \
  st3 --endpoint "$socket" status "$agent" --json
old_incarnation="$(st3 --endpoint "$socket" status "$agent" --json \
  | jq -er '.subjects[0].actual.incarnation_id')"
st3 --endpoint "$socket" claim "$agent" harness.observed --actor "$agent" \
  --field state=ready --field driver=codex --field incarnation_id="$old_incarnation" \
  --idempotency-key eval-old-ready >/dev/null
st3 --endpoint "$socket" pty signal "$agent" hangup >/dev/null

await_jq 20 \
  ".subjects[0].actual.status == \"running\" and .subjects[0].actual.incarnation_id != \"$old_incarnation\"" \
  st3 --endpoint "$socket" status "$agent" --json
new_incarnation="$(st3 --endpoint "$socket" status "$agent" --json \
  | jq -er '.subjects[0].actual.incarnation_id')"
st3 --endpoint "$socket" status "$agent" --json \
  | jq -e --arg current "$new_incarnation" \
      '.subjects[0].harness == null
       or (.subjects[0].harness.incarnation_id == $current
           and .subjects[0].harness.state != "ready")' \
      >/dev/null

await_jq 70 'map(select(.title == "An agent harness did not become ready")) | length == 1' \
  st3 --endpoint "$socket" attention ls --as person/operator --json
st3 --endpoint "$socket" inspect "$agent" --json \
  | jq -e '.recent_claims | map(.kind) | index("runtime.readiness-deadline-reached") != null' \
  >/dev/null

st3 --endpoint "$socket" claim "$agent" harness.observed --actor "$agent" \
  --field state=ready --field driver=codex --field incarnation_id="$new_incarnation" \
  --idempotency-key eval-new-ready >/dev/null
await_jq 10 'map(select(.title == "An agent harness did not become ready")) | length == 0' \
  st3 --endpoint "$socket" attention ls --as person/operator --json

st3 --endpoint "$socket" status "$agent" --json \
  | jq -e --arg current "$new_incarnation" \
      '.subjects[0].harness.state == "ready" and .subjects[0].harness.incarnation_id == $current' \
      >/dev/null
st3 --endpoint "$socket" message ls "$agent" --archive --json \
  | jq -e '[.[] | select(.tags[]? | startswith("st3-work:"))] as $work
      | ($work | length) == 2
      and ([$work[] | select(.status == "closed")] | length) == 1
      and ([$work[] | select(.status == "sent" or .status == "delivered")] | length) == 1' >/dev/null

printf 'RUNTIME-READINESS-RECOVERY-GREEN\n' >result.txt
