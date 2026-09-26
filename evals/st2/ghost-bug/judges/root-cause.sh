#!/usr/bin/env bash
# JUDGE: root cause — the shared-default mutation is really gone, proven by two probes BLIND to how it was
# fixed:
#   (a) BEHAVIOR — a custom-options call followed by a default call returns "[ b ]" (the bug returned "< b >").
#   (b) NO-MUTATION — a custom call leaves defaultOptions UNCHANGED. This is the actual root cause; a band-aid
#       that freezes/resets/re-declares the defaults still fails (b) or the visible suite.
# A freeze/try-catch/reassign pattern in format.js is surfaced as a WARN smell (non-gating).
#
# PASS (exit 0): both probes hold.
set -uo pipefail
ROOT="${CATALOG:-$PWD}"
W="$ROOT/worker"
[ -d "$W" ] || { echo "FAIL: no worker repo at $W"; exit 1; }
fail=0

BEH=$(cd "$W" && node --input-type=module -e '
import { format } from "./src/format.js";
format("a", { prefix: "<", suffix: ">" });      // a custom call first
console.log(format("b"));                        // buggy -> "< b >"; correct -> "[ b ]"
' 2>&1)
[ "$BEH" = "[ b ]" ] && echo "PASS: BEHAVIOR — format(custom) then format(default) === '[ b ]' (bug fixed)" \
                     || { echo "FAIL: BEHAVIOR — format(custom) then format(default) === '$BEH' (expected '[ b ]' — bug still reproduces)"; fail=1; }

NOMUT=$(cd "$W" && node --input-type=module -e '
import { defaultOptions } from "./src/config.js";
import { format } from "./src/format.js";
const before = JSON.stringify(defaultOptions);
format("x", { prefix: "<", suffix: ">", pad: 3 });
const after = JSON.stringify(defaultOptions);
console.log(before === after ? "UNCHANGED" : "MUTATED("+before+"->"+after+")");
' 2>&1)
echo "$NOMUT" | grep -qx UNCHANGED && echo "PASS: NO-MUTATION — a custom call leaves defaultOptions UNCHANGED (root cause fixed)" \
                                   || { echo "FAIL: NO-MUTATION — defaultOptions was mutated by a custom call — $NOMUT"; fail=1; }

# paper-over smell (signal, non-gating): a freeze/try-catch/reset/re-clone inside format is a band-aid tell.
if grep -qE 'Object\.freeze|try[[:space:]]*\{|catch|defaultOptions[[:space:]]*=' "$W/src/format.js" 2>/dev/null; then
  echo "  WARN: src/format.js contains a freeze/try-catch/reassign pattern — check the fix is a non-mutating MERGE, not a band-aid"
fi
exit "$fail"
