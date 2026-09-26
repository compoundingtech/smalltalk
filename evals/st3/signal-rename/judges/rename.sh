#!/usr/bin/env bash
# RENAME CORRECTNESS (hard gate): the PRODUCT is renamed to `beacon` (package name, scheme, protocol) with NO
# product `signal` token left. These tokens are unambiguously the product (they never occur in the primitive).
set -uo pipefail
. "$(dirname "$0")/_integrate.sh"
fail=0
chk(){ eval "$1" && echo "  ok: $2" || { echo "  FAIL: $3"; fail=1; }; }
chk 'wgrep -q "@acme/beacon" -- "*package.json"'           "package name/peerDep uses @acme/beacon"   "no @acme/beacon in any package.json (base rename missing)"
chk '! wgrep -q "@acme/signal" -- "*package.json"'         "no lingering @acme/signal name/peerDep"    "lingering @acme/signal in a package.json (rename incomplete)"
chk 'wgrep -qE "beacon:(//|[\"'\''])"'                     "address scheme renamed to beacon:"        "beacon: scheme not found (hub SCHEME / relay ACCEPT_SCHEME not renamed)"
chk '! wgrep -qE "signal:(//|[\"'\''])"'                   "no lingering signal:// scheme"            "lingering signal:// / \"signal:\" scheme (under-rename)"
chk 'wgrep -q "beacon/1"'                                  "protocol tag renamed to beacon/1"         "no beacon/1 protocol tag (base PROTOCOL not renamed)"
chk '! wgrep -q "signal/1"'                                "no lingering signal/1 protocol tag"       "lingering signal/1 protocol tag"
[ "$fail" -eq 0 ] || { echo "FAIL: product rename incomplete/inconsistent (a blind or partial rename)"; exit 1; }
echo "PASS: product fully renamed to beacon (package + scheme + protocol), no lingering product 'signal' token"
