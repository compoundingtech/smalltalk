#!/usr/bin/env bash
set -euo pipefail
export NIX_CONFIG='builders = '
export CARGO_BUILD_JOBS=8
export GIT_CONFIG_GLOBAL=/nix/store/18f9jjdp0mfidirvqfg265xhbmzrc867-agent-policy-gitconfig
ST=/home/schickling/.megarepo/github.com/compoundingtech/smalltalk/refs/heads/schickling-assistant/2026-10-10-status-bakeoff-st
WS=/home/schickling/.megarepo/github.com/schickling/dotfiles/refs/heads/schickling-assistant/2026-10-10-status-bakeoff-ws
LP=/home/schickling/.megarepo/github.com/schickling/dotfiles/refs/heads/schickling-assistant/2026-10-10-status-bakeoff-lp
OUT=/tmp/status-bakeoff-artifacts
mkdir -p "$OUT"
cd "$ST"
export CARGO_TARGET_DIR=/tmp/status-bakeoff-target-st
nix develop --option builders '' --command bash --noprofile --norc -c '
set -euo pipefail
cargo run -p st3-client-codegen
cargo build -p st3 --bin st3 --example status_bakeoff_inject
cargo test -p st3 --lib agents_poll_
cargo test -p st3 --lib agents_publication_local_only
cargo test -p st3 --lib an_idle_agents_publication_poll
printf "%s\n" "$ST3_OTELITE_BIN" > /tmp/status-bakeoff-artifacts/otelite-path
'
cp "$CARGO_TARGET_DIR/debug/st3" "$OUT/st3"
cp "$CARGO_TARGET_DIR/debug/examples/status_bakeoff_inject" "$OUT/status_bakeoff_inject"
export CARGO_TARGET_DIR=/tmp/status-bakeoff-target-fractal
cd "$WS"
nix develop --option builders '' ./flakes/fractal --command bash --noprofile --norc -c 'set -euo pipefail; cargo build --manifest-path flakes/fractal/Cargo.toml -p fractal; cargo test --manifest-path flakes/fractal/Cargo.toml -p fractal --bin fractal ws_'
cp "$CARGO_TARGET_DIR/debug/fractal" "$OUT/fractal-ws"
cd "$LP"
nix develop --option builders '' ./flakes/fractal --command bash --noprofile --norc -c 'set -euo pipefail; cargo build --manifest-path flakes/fractal/Cargo.toml -p fractal; cargo test --manifest-path flakes/fractal/Cargo.toml -p fractal --bin fractal agents_lp::tests; cargo test --manifest-path flakes/fractal/Cargo.toml -p fractal --bin fractal lp_source_fences'
cp "$CARGO_TARGET_DIR/debug/fractal" "$OUT/fractal-lp"
printf 'BAKEOFF_BUILD_COMPLETE\n'
