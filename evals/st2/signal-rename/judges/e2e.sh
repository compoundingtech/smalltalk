#!/usr/bin/env bash
# HELD-OUT E2E (hard gate): a rename-AGNOSTIC driver resolves the renamed base+relay+hub end-to-end (hub hosts →
# relay moves it scheme-checked → resolve the known value). It uses only identifiers the product rename does NOT
# touch (hostAndResolve, Relay, a known value), so it survives Signal→Beacon / signal://→beacon://; it fails iff
# the rename is INCONSISTENT across packages (e.g. hub SCHEME != relay ACCEPT_SCHEME → relay refuses → throws).
set -uo pipefail
. "$(dirname "$0")/_integrate.sh"
HUB=$(   cd "$W" && for d in */; do [ -f "${d}package.json" ] && grep -q '"name":[^,]*-hub"'   "${d}package.json" && { echo "${d%/}"; break; }; done )
RELAY=$( cd "$W" && for d in */; do [ -f "${d}package.json" ] && grep -q '"name":[^,]*-relay"' "${d}package.json" && { echo "${d%/}"; break; }; done )
[ -n "$HUB" ] && [ -n "$RELAY" ] || { echo "FAIL: could not locate the renamed hub (*-hub) and/or relay (*-relay) — rename likely incomplete"; exit 1; }
drv="$W/.e2e-driver.mjs"
cat > "$drv" <<JS
import assert from "node:assert/strict";
import { hostAndResolve } from "./$HUB/src/hub.js";
import { Relay } from "./$RELAY/src/relay.js";
const KNOWN = "known-42";
const v = await hostAndResolve({ Relay, host: "alpha", topic: "greeting", value: KNOWN });
assert.equal(v, KNOWN);
console.log("e2e ok:", v);
JS
if ( cd "$W" && node .e2e-driver.mjs >/dev/null 2>&1 ); then
  rm -f "$drv"; echo "PASS: held-out e2e GREEN — renamed base+relay+hub resolve consistently end-to-end"
else
  rm -f "$drv"; echo "FAIL: held-out e2e RED — rename incomplete/inconsistent (scheme mismatch, unresolved shim, or broken seam)"; exit 1
fi
