#!/usr/bin/env bash
set -euo pipefail

assets="$PWD"
root="$(mktemp -d "${TMPDIR:-/tmp}/st3-machine-stream.XXXXXX")"
pty_root="$root/machine-pty"
remote_socket="$root/remote.sock"
proxy_socket="$root/proxy.sock"
stream="$root/attach.stream"
stdout="$root/attach.stdout"
stderr="$root/attach.stderr"
attach_pid=""; remote_pid=""; proxy_pid=""
export PTY_ROOT="$pty_root"
pty_at() { env -u PTY_SESSION PTY_ROOT="$PTY_ROOT" pty "$@"; }
cleanup() {
  if test -n "$attach_pid"; then kill "$attach_pid" >/dev/null 2>&1 || true; wait "$attach_pid" >/dev/null 2>&1 || true; fi
  for pid in "$proxy_pid" "$remote_pid"; do if test -n "$pid"; then kill "$pid" >/dev/null 2>&1 || true; wait "$pid" >/dev/null 2>&1 || true; fi; done
  pty_at kill ms.target >/dev/null 2>&1 || true
  pty_at rm ms.target >/dev/null 2>&1 || true
}
trap cleanup EXIT
wait_for() {
  local description="$1"; shift
  for _ in $(seq 1 200); do "$@" && return; sleep 0.05; done
  printf 'timed out waiting for %s\n' "$description" >&2
  return 1
}
peek_has() { pty_at peek --plain ms.target 2>/dev/null | grep -Fq "$1"; }

pty_at run -d --id ms.target --no-display-name -- bash "$assets/target.sh"
wait_for "target output" peek_has INITIAL_COLOR_61e8
env -u PTY_SESSION PTY_ROOT="$PTY_ROOT" pty remote-serve --socket "$remote_socket" >"$root/remote.log" 2>&1 &
remote_pid="$!"
wait_for "remote socket" test -S "$remote_socket"
node "$assets/drop-proxy.mjs" "$proxy_socket" "$remote_socket" "$root/drop-first" >"$root/proxy.log" 2>&1 &
proxy_pid="$!"
wait_for "proxy socket" test -S "$proxy_socket"
PTY_EVAL_STATE="$root/fabric-state" PTY_EVAL_SOCKET_1="$proxy_socket" PTY_EVAL_SOCKET_2="$remote_socket" PTY_FABRIC_BIN="$assets/fabric" env -u PTY_SESSION PTY_ROOT="$PTY_ROOT" pty attach --remote eval-peer --attach-stream-fd-v1 3 ms.target 3>"$stream" >"$stdout" 2>"$stderr" &
attach_pid="$!"
wait_for "initial snapshot" node "$assets/check-stream.mjs" "$stream" snapshots 1
touch "$root/drop-first"
wait_for "second dial" test -f "$root/fabric-state/second-dial"
pty_at send ms.target --seq AFTER_DROP_61e8 --seq key:return
wait_for "post-drop output" peek_has AFTER_DROP_61e8
touch "$root/fabric-state/release-second"
wait_for "reconnect snapshot" node "$assets/check-stream.mjs" "$stream" snapshots 2
pty_at send ms.target --seq EXIT_61e8 --seq key:return
wait_for "attach exit" sh -c "! kill -0 '$attach_pid' 2>/dev/null"
wait "$attach_pid"
attach_pid=""
node "$assets/check-stream.mjs" "$stream" final >"$root/stream-proof"
test ! -s "$stdout"
! grep -Fq INITIAL_COLOR_61e8 "$stderr"
cat "$root/stream-proof"
for pid in "$proxy_pid" "$remote_pid"; do kill "$pid" >/dev/null 2>&1 || true; wait "$pid" >/dev/null 2>&1 || true; done
proxy_pid=""; remote_pid=""
pty_at kill ms.target >/dev/null 2>&1 || true
pty_at rm ms.target >/dev/null 2>&1 || true
trap - EXIT
test "$(pty_at list --json | jq 'length')" -eq 0
echo MACHINE-STREAM-CLEANUP-GREEN-61e8
rm -rf "$root"
