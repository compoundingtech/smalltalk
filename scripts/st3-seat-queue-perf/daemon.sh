# Shared helpers for the seat-queue performance scripts. Source this file.
#
# Each daemon is isolated: its own config, state directory, sockets, and PTY
# registry under ROOT, no peers, and a clean login environment. ROOT must be a
# short path because it holds Unix sockets. Set PERF_DATA to keep the large
# claim stores on a disk directory instead of under ROOT. Set PERF_DAEMON_ENV to
# space-separated NAME=VALUE pairs to add to the daemon's environment, such as
# an allocator preload.

perf_state_dir() {
  if [[ -n ${PERF_DATA:-} ]]; then
    printf '%s/%s\n' "$PERF_DATA" "$(basename "$1")"
  else
    printf '%s/state\n' "$1"
  fi
}

perf_login_path() {
  env -i HOME="$HOME" USER="$USER" LOGNAME="$USER" SHELL="$SHELL" LANG="${LANG:-C.UTF-8}" \
    PATH=/usr/bin:/bin "$SHELL" -l -i -c 'printf %s "$PATH"' 2>/dev/null
}

# perf_start_daemon ST3_BIN ROOT: start one niced daemon and wait for its socket.
perf_start_daemon() {
  local st3_bin=$1 root=$2 bin_dir state
  bin_dir="$root/bin"
  state="$(perf_state_dir "$root")"
  mkdir -p "$bin_dir" "$state" "$root/pty"
  cp "$st3_bin" "$bin_dir/st3"
  cat >"$root/config.toml" <<TOML
node = "perfnode"
person = "person/perf-operator"
state_dir = "$state"
socket = "$root/st3.sock"
client_gateway_socket = "$root/client.sock"
TOML
  # shellcheck disable=SC2086
  env -i HOME="$HOME" USER="$USER" LOGNAME="$USER" SHELL="$SHELL" LANG="${LANG:-C.UTF-8}" \
    PATH="$bin_dir:$(perf_login_path)" ${PERF_DAEMON_ENV:-} \
    setsid nice -n 10 "$bin_dir/st3" up --config "$root/config.toml" --pty-root "$root/pty" \
    >>"$root/daemon.log" 2>&1 </dev/null &
  echo $! >"$root/daemon.pid"
  local _
  for _ in $(seq 1 300); do
    [[ -S "$root/st3.sock" ]] && return 0
    sleep 0.2
  done
  echo "the daemon under $root did not open its socket" >&2
  return 1
}

# perf_stop_daemon ROOT: stop the daemon, its PTYs, and any process left in ROOT.
perf_stop_daemon() {
  local root=$1 pid session proc other ancestor signal
  pid="$(cat "$root/daemon.pid")"
  PTY_ROOT="$root/pty" pty list --json 2>/dev/null | jq -r '.[].name' | while read -r session; do
    PTY_ROOT="$root/pty" pty kill "$session" >/dev/null 2>&1 || true
  done
  kill -TERM "$pid" 2>/dev/null || true
  for _ in $(seq 1 100); do kill -0 "$pid" 2>/dev/null || break; sleep 0.2; done
  declare -A ancestors=()
  ancestor=$$
  while [[ -n "$ancestor" && "$ancestor" != 0 ]]; do
    ancestors[$ancestor]=1
    ancestor="$(awk '/^PPid:/ {print $2}' "/proc/$ancestor/status" 2>/dev/null || true)"
  done
  for signal in TERM KILL; do
    for proc in /proc/[0-9]*; do
      other="${proc#/proc/}"
      [[ -n "${ancestors[$other]:-}" ]] && continue
      if { tr '\0' ' ' <"$proc/cmdline"; } 2>/dev/null | grep -qF "$root/bin/" \
        || [[ "$(readlink "$proc/cwd" 2>/dev/null)" == "$root/"* ]] \
        || { tr '\0' '\n' <"$proc/environ"; } 2>/dev/null | grep -qxF "ST3_ENDPOINT=$root/st3.sock"; then
        kill "-$signal" "$other" 2>/dev/null || true
      fi
    done
    sleep 1
  done
}
