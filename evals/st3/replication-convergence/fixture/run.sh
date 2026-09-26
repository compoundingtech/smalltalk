#!/usr/bin/env bash
set -euo pipefail

root="$(mktemp -d "${TMPDIR:-/tmp}/st3-replication.XXXXXX")"
export XDG_CONFIG_HOME="$root/config"
declare -a daemons=()
declare -a workers=()

cleanup() {
  local status=$?
  for pid in "${workers[@]:-}" "${daemons[@]:-}"; do
    kill -TERM "$pid" 2>/dev/null || true
  done
  for pid in "${workers[@]:-}" "${daemons[@]:-}"; do
    wait "$pid" 2>/dev/null || true
  done
  if (( status != 0 )); then
    for log in "$root"/*/*.log; do
      if [[ -f "$log" ]]; then
        printf '\n== %s ==\n' "$log" >&2
        sed -n '1,240p' "$log" >&2
      fi
    done
  fi
  rm -rf "$root"
}
trap cleanup EXIT

read -r port_a port_b < <(python3 - <<'PY'
import socket
sockets = []
for _ in range(2):
    item = socket.socket()
    item.bind(("127.0.0.1", 0))
    sockets.append(item)
print(*(item.getsockname()[1] for item in sockets))
PY
)

fleet="1f91ca65-7793-48cc-866e-ac15690130e1"
secret="$root/fleet.secret"
python3 - "$secret" <<'PY'
import pathlib
import sys
pathlib.Path(sys.argv[1]).write_bytes(bytes(range(32)))
PY
chmod 600 "$secret"

for name in a b; do
  mkdir -p "$root/$name/state"
done
socket_a="$root/a/st3.sock"
socket_b="$root/b/st3.sock"

common_a=(--node replica-a --state-dir "$root/a/state" --socket "$socket_a" --fleet-id "$fleet" --shared-secret-file "$secret" --peer-listen "127.0.0.1:$port_a" --peer "replica-b=http://127.0.0.1:$port_b")
common_b=(--node replica-b --state-dir "$root/b/state" --socket "$socket_b" --fleet-id "$fleet" --shared-secret-file "$secret" --peer-listen "127.0.0.1:$port_b" --peer "replica-a=http://127.0.0.1:$port_a")

st3 up "${common_a[@]}" >"$root/a/daemon.log" 2>&1 &
daemons+=("$!")
st3 up "${common_b[@]}" >"$root/b/daemon.log" 2>&1 &
daemons+=("$!")
st3 replication-worker "${common_a[@]}" >"$root/a/worker.log" 2>&1 &
workers+=("$!")
st3 replication-worker "${common_b[@]}" >"$root/b/worker.log" 2>&1 &
workers+=("$!")

python3 - "$socket_a" "$socket_b" <<'PY'
import os
import sys
import time
deadline = time.monotonic() + 10
while time.monotonic() < deadline:
    if all(os.path.exists(path) for path in sys.argv[1:]):
        sys.exit(0)
    time.sleep(0.05)
raise SystemExit("the local APIs did not become ready")
PY

printf 'version 2\nresource "initial-a" { kind "custom.eval.initial-a" }\n' >"$root/initial-a.kdl"
st3 --endpoint "$socket_a" publish "$root/initial-a.kdl" --as person/eval >/dev/null
python3 ./await_convergence.py "$socket_a" "$socket_b" 10 >/dev/null
printf 'INITIAL-CONVERGENCE\n' >result.txt

kill -TERM "${workers[0]}"
wait "${workers[0]}" 2>/dev/null || true
st3 --endpoint "$socket_a" doctor >/dev/null
printf 'MAIN-SURVIVED\n' >>result.txt

printf 'version 2\nresource "partition-a" { kind "custom.eval.partition-a" }\n' >"$root/partition-a.kdl"
printf 'version 2\nresource "partition-b" { kind "custom.eval.partition-b" }\n' >"$root/partition-b.kdl"
st3 --endpoint "$socket_a" publish "$root/partition-a.kdl" --as person/eval >/dev/null
st3 --endpoint "$socket_b" publish "$root/partition-b.kdl" --as person/eval >/dev/null

st3 replication-worker "${common_a[@]}" >>"$root/a/worker.log" 2>&1 &
workers[0]="$!"
python3 ./await_convergence.py "$socket_a" "$socket_b" 15 >/dev/null
st3 --endpoint "$socket_a" inspect resource/partition-b --json >/dev/null
st3 --endpoint "$socket_b" inspect resource/partition-a --json >/dev/null
printf 'PARTITION-HEALED\n' >>result.txt

cleanup
trap - EXIT
