#!/usr/bin/env bash
# st3 answer to a Codex Stop hook, kept for workspaces whose .codex/hooks.json still names it. The
# st3 driver delivers messages to the running session, so this never blocks the stop. It never
# looks a program up on PATH, and a seat without its st3 binary says so on stderr instead of
# passing silently.

set -u

if [[ -z "${ST3_BIN:-}" || ! -x "$ST3_BIN" ]]; then
  cat >/dev/null
  echo "st: cannot answer Codex stop: this seat's st3 binary is missing (ST3_BIN=${ST3_BIN:-unset})" >&2
  exit 1
fi

exec "$ST3_BIN" driver-hook codex-stop "$@"
