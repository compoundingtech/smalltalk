#!/usr/bin/env bash
set -euo pipefail

root="$(mktemp -d "${TMPDIR:-/tmp}/st3-attach-only.XXXXXX")"
candidate="attach-only-dead"
control="legacy-dead"
live="attach-only-live"
mkdir -p "$root"
pty_at() { PTY_ROOT="$root" env -u PTY_SESSION pty "$@"; }
cleanup() {
  for id in "$candidate" "$control" "$live"; do
    pty_at kill "$id" >/dev/null 2>&1 || true
    pty_at rm "$id" >/dev/null 2>&1 || true
  done
  rm -rf "$root"
}
trap cleanup EXIT
wait_status() {
  local id="$1" want="$2" status=""
  for _ in $(seq 1 100); do
    status="$(pty_at list --json | jq -r --arg id "$id" '.[] | select(.name == $id) | .status')"
    [ "$status" = "$want" ] && return
    sleep 0.05
  done
  return 1
}
count_runs() { test -f "$1" && wc -l <"$1" || printf '0\n'; }
count_starts() { jq -s '[.[] | select(.type == "session_start")] | length' "$root/$1.events.jsonl"; }

pty attach --help | grep -Eq '(^|[[:space:]])--no-restart([[:space:]]|$)'
echo ATTACH-ONLY-SURFACE-GREEN-7ca1

live_marker="$CATALOG/live-runs"
pty_at run -d --id "$live" --tag keep=true -- bash "$PWD/live.sh" "$live_marker"
wait_status "$live" running
for _ in $(seq 1 100); do pty_at peek --plain "$live" 2>/dev/null | grep -Fq ATTACH-ONLY-LIVE-READY && break; sleep 0.05; done
set +e
printf 'live-input\n' | timeout 10 script -qefc "env -u PTY_SESSION PTY_ROOT='$root' pty attach --no-restart '$live'" /dev/null >"$CATALOG/live.transcript" 2>&1
live_rc=$?
set -e
test "$live_rc" -ne 124
grep -Fq LIVE-ACK:live-input "$CATALOG/live.transcript"
wait_status "$live" exited
test "$(count_runs "$live_marker")" -eq 1
test "$(count_starts "$live")" -eq 1
echo LIVE-ATTACH-ROUNDTRIP-GREEN-7ca1

control_marker="$CATALOG/control-runs"
pty_at run -d --id "$control" --tag keep=true -- bash "$PWD/once.sh" "$control_marker"
wait_status "$control" exited
set +e
printf 'future-input\n' | timeout 10 script -qefc "env -u PTY_SESSION PTY_ROOT='$root' pty attach '$control'" /dev/null >"$CATALOG/control.transcript" 2>&1
control_rc=$?
set -e
test "$control_rc" -ne 124
wait_status "$control" exited
grep -Fq 'Restart? [Y/n]' "$CATALOG/control.transcript"
test "$(count_runs "$control_marker")" -eq 2
echo LEGACY-RESTART-CONTROL-GREEN-7ca1

candidate_marker="$CATALOG/candidate-runs"
pty_at run -d --id "$candidate" --tag keep=true -- bash "$PWD/once.sh" "$candidate_marker"
wait_status "$candidate" exited
test "$(count_runs "$candidate_marker")" -eq 1
test "$(count_starts "$candidate")" -eq 1
set +e
printf 'future-input\n' | timeout 10 script -qefc "env -u PTY_SESSION PTY_ROOT='$root' pty attach --no-restart '$candidate'" /dev/null >"$CATALOG/candidate.transcript" 2>&1
candidate_rc=$?
set -e
test "$candidate_rc" -ne 0
test "$candidate_rc" -ne 124
grep -Fq "Session \"$candidate\" is not running (status: exited)." "$CATALOG/candidate.transcript"
! grep -Fq 'Restart? [Y/n]' "$CATALOG/candidate.transcript"
echo DEAD-ATTACH-REFUSAL-GREEN-7ca1
test "$(count_runs "$candidate_marker")" -eq 1
test "$(count_starts "$candidate")" -eq 1
echo NO-NEW-INCARNATION-GREEN-7ca1
test "$(pty_at list --json | jq -r --arg id "$candidate" '.[] | select(.name == $id) | .status')" = exited
echo DEAD-STATE-UNCHANGED-GREEN-7ca1

cleanup
trap - EXIT
test "$(pty_at list --json | jq 'length')" -eq 0
echo SYNTHETIC-ROOT-CLEAN-GREEN-7ca1
rm -rf "$root"
