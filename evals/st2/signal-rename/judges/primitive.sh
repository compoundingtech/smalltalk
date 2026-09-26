#!/usr/bin/env bash
# PRIMITIVE INTACT (hard gate — the trap): the runtime primitive (AbortSignal / controller.signal / SIGTERM) must
# survive undamaged in the relay package. A blind s/signal/beacon/g renames the primitive and fails here.
set -uo pipefail
. "$(dirname "$0")/_integrate.sh"
RELAY=$( cd "$W" && for d in */; do [ -f "${d}package.json" ] && grep -q '"name":[^,]*-relay"' "${d}package.json" && { echo "${d%/}"; break; }; done )
[ -n "$RELAY" ] || { echo "FAIL: could not locate the relay package (*-relay) — cannot check the primitive"; exit 1; }
# git grep the COMMITTED relay code (tracked files only), so the eval persona overlay never pollutes.
rgrep(){ ( cd "$W" && git grep -In "$1" -- "$RELAY" 2>/dev/null ); }
fail=0
rgrep 'AbortSignal'        >/dev/null && echo "  ok: AbortSignal present (primitive kept)"          || { echo "  FAIL: AbortSignal MISSING from $RELAY — primitive renamed (blind find-replace)"; fail=1; }
rgrep 'controller\.signal' >/dev/null && echo "  ok: controller.signal present (AbortController intact)" || { echo "  FAIL: controller.signal MISSING/renamed in $RELAY"; fail=1; }
rgrep 'SIGTERM'            >/dev/null && echo "  ok: SIGTERM handler present (OS-signal primitive kept)"   || { echo "  FAIL: SIGTERM MISSING from $RELAY — OS-signal handling renamed"; fail=1; }
# The positive checks above already catch a blind rename (AbortSignal→AbortBeacon makes AbortSignal MISSING).
# `AbortBeacon` is the one UNAMBIGUOUS corruption artifact — it can only result from renaming AbortSignal and
# never appears in the trap's explanatory prose ("...into a beacon name"), unlike a loose SIGTERM.*beacon match.
if rgrep 'AbortBeacon' >/dev/null; then echo "  FAIL: AbortBeacon token in $RELAY (AbortSignal was blindly renamed)"; fail=1; else echo "  ok: no AbortBeacon corruption token"; fi
[ "$fail" -eq 0 ] || { echo "FAIL: the runtime primitive was renamed/damaged — the trap was hit"; exit 1; }
echo "PASS: the primitive (AbortSignal / controller.signal / SIGTERM) is intact + undamaged in $RELAY"
