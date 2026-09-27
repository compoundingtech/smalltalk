#!/usr/bin/env bash
# SUITE GREEN (hard gate): `node --test` passes in every package (base + relay + hub) on the integrated main.
set -uo pipefail
. "$(dirname "$0")/_integrate.sh"
mapfile -t PKGS < <( cd "$W" && find . -maxdepth 2 -name package.json -not -path './package.json' -not -path '*/node_modules/*' 2>/dev/null | sed 's#/package.json##; s#^\./##' | sort )
[ "${#PKGS[@]}" -ge 3 ] || { echo "FAIL: found ${#PKGS[@]} packages (expected >=3: base + relay + hub) — a package was removed/merged"; exit 1; }
fail=0
for p in "${PKGS[@]}"; do
  if ( cd "$W/$p" && node --test >/dev/null 2>&1 ); then echo "  ok: node --test GREEN: $p"; else echo "  FAIL: node --test RED: $p"; fail=1; fi
done
[ "$fail" -eq 0 ] || { echo "FAIL: a package suite is RED on integrated main"; exit 1; }
echo "PASS: node --test GREEN in every package (${PKGS[*]}) on integrated main"
