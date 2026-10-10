#!/usr/bin/env bash
set -euo pipefail
export NIX_CONFIG='builders = '
cd /home/schickling/.megarepo/github.com/compoundingtech/smalltalk/refs/heads/schickling-assistant/2026-10-10-status-bakeoff-st
exec env -u BASH_ENV -u ENV /home/schickling/.local/share/gate-slot/bin/gate-slot --class gate -- nix develop --option builders '' --command bash --noprofile --norc -c 'printf "OTELITE=%s\n" "$ST3_OTELITE_BIN"; cargo --version; rustc --version'
