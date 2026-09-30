#!/usr/bin/env bash
# st3 answer to Claude's StopFailure reporting hook, kept for workspaces whose own settings still
# name it. claude-observe.sh records the failed turn and its credential diagnostic, and the driver
# publishes the seat's state, so this adds nothing. It never looks a program up on PATH, and a
# seat without its st3 binary says so on stderr instead of passing silently.

set -u

if [[ -z "${ST3_BIN:-}" || ! -x "$ST3_BIN" ]]; then
  cat >/dev/null
  echo "st: cannot answer Claude stop-failure: this seat's st3 binary is missing (ST3_BIN=${ST3_BIN:-unset})" >&2
  exit 1
fi

exec "$ST3_BIN" driver-hook claude-stop-failure "$@"
