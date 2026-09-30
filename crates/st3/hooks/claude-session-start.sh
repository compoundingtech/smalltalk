#!/usr/bin/env bash
# st3 answer to Claude's SessionStart context hook, kept for workspaces whose own settings still
# name it. st3 seats take no startup turn, so st3 adds no context. It never looks a program up on
# PATH, and a seat without its st3 binary says so on stderr instead of passing silently.

set -u

if [[ -z "${ST3_BIN:-}" || ! -x "$ST3_BIN" ]]; then
  cat >/dev/null
  echo "st: cannot answer Claude session-start: this seat's st3 binary is missing (ST3_BIN=${ST3_BIN:-unset})" >&2
  exit 1
fi

exec "$ST3_BIN" driver-hook claude-session-start "$@"
