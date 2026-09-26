#!/usr/bin/env bash
# st2 Claude status-line tee: record the status-line payload (stdin JSON) to the agent's
# harness-context record, then hand the SAME bytes to the operator's own renderer.
#
# Claude's `statusLine` is a single slot and the winning declaration replaces the others outright
# — it does not merge — so st2 occupying it in `.claude/settings.local.json` would silently remove
# whatever the operator had on every managed seat. Chaining is therefore mandatory (HC-R18).
#
# Where the tee cannot chain it renders NOTHING, never the payload. The payload is machine JSON —
# session id, transcript path, model and usage blocks — so echoing it into the status-line slot
# paints a wall of JSON across the operator's terminal every five seconds. That is strictly worse
# for them than an empty line, and there is nothing in it they can act on. Recording is unaffected
# either way; only the human-facing line is at stake. This arm is the outermost degradation:
#
#   - no identity, no catalog root, or no `st2` on PATH -> drain stdin, print nothing
#   - `st2` present -> it records (fail-open) and chains; it never exits non-zero on its own
#
# The fallback drains rather than exiting, because Claude writes the payload to this process's
# stdin and an exit that never reads it would hand Claude an EPIPE every five seconds. `exec` in
# both arms deliberately: no command substitution anywhere, so the stdin bytes reach the
# downstream renderer unchanged rather than through the shell's trailing-newline stripping.
#
# Known limitation, accepted: `exec` leaves no fallback for an `st2` that RUNS but rejects
# `claude-statusline` — an old binary against a new hook set during an upgrade — which exits
# non-zero having printed nothing. Since a blank line is now what EVERY degraded arm produces,
# that case is no longer an anomaly worth buying a fallback for: the probe subprocess it would
# cost recurs on every render, at 5-second cadence, forever. The window is a single upgrade, and
# materialization refuses this registration at all unless the hook set verifies.

set -u

identity="${ST2_CLAUDE_IDENTITY:-${ST_AGENT:-}}"
# CATALOG-first, matching claude-observe.sh: `--catalog` resolves the agent DECLARATION, and with
# a custom bus root (ST_ROOT != CATALOG) resolution under ST_ROOT would find nothing.
root="${CATALOG:-${ST_ROOT:-}}"
if [[ -z "$identity" || -z "$root" ]] || ! command -v st2 >/dev/null 2>&1; then
  exec cat >/dev/null
fi

exec st2 --catalog "$root" driver claude-statusline --identity "$identity"
