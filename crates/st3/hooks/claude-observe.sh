#!/usr/bin/env bash
# st3 Claude hook: hand one lifecycle event (name in $1, payload on stdin) to the st3 binary that
# launched this seat. It never looks a program up on PATH. A seat without its binary can neither
# record its turns nor bind its transcript, so this says so on stderr and exits non-zero instead
# of passing silently; the driver also records a harness.diagnostic when no binding appears.

set -u

if [[ -z "${ST3_BIN:-}" || ! -x "$ST3_BIN" ]]; then
  cat >/dev/null
  echo "st: cannot record Claude ${1:-hook}: this seat's st3 binary is missing (ST3_BIN=${ST3_BIN:-unset})" >&2
  exit 1
fi

exec "$ST3_BIN" driver-hook claude-observe "$@"
