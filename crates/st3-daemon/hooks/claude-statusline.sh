#!/usr/bin/env bash
# st3 Claude status line: record the status-line payload (stdin JSON) in the seat's harness
# context, then hand the same bytes to the operator's own renderer. Claude's statusLine is a
# single slot, so st3 chains rather than replaces it. It never looks a program up on PATH.
# Without this seat's st3 binary it drains the payload and shows the fault as the status line.

set -u

if [[ -z "${ST3_BIN:-}" || ! -x "$ST3_BIN" ]]; then
  cat >/dev/null
  echo "st: hooks cannot run: this seat's st3 binary is missing (ST3_BIN=${ST3_BIN:-unset})"
  exit 0
fi

exec "$ST3_BIN" driver-hook claude-statusline "$@"
