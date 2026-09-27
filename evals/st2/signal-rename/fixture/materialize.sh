#!/usr/bin/env bash
# Materialize the signal-rename workspace inside the eval catalog ($CATALOG), as the eval's `run { step }` BEFORE
# the team boots. Builds: a seed workspace from the bundled synthetic graph (signal + signal-relay + signal-hub +
# config/app.toml), a BARE origin, and one full clone per agent (sup/base/relay/hub) with DISTINCT authors so
# isolation is attributable. The cross-repo acceptance test is HELD OUT (no agent sees it). Each Codex seat gets
# its source persona as AGENTS.md. Each agent gets the FULL workspace (so the relative _signal.js shims + the
# integration resolve with ZERO node_modules) but commits only its lane. Clones carry absolute origin URLs, so
# this must run at eval time (a static fixture can't).
set -euo pipefail
SB="${CATALOG:?CATALOG must be set — st2 eval provides it to run steps}"
cd "$SB"
GRAPH="$SB/seed-graph"           # copied in from the fixture
SEED="$SB/.seed"

rm -rf "$SEED" "$SB"/origin.git "$SB"/sup "$SB"/base "$SB"/relay "$SB"/hub "$SB"/.held-out
mkdir -p "$SEED"

echo "== seed: assemble the workspace (signal + signal-relay + signal-hub + config) =="
cp -R "$GRAPH/signal" "$GRAPH/signal-relay" "$GRAPH/signal-hub" "$SEED/"
mkdir -p "$SEED/config"
cp "$GRAPH/app.toml" "$SEED/config/app.toml"

echo "== HOLD OUT the cross-repo acceptance test (no agent may see it) =="
mkdir -p "$SB/.held-out"
mv "$SEED/signal-hub/test/integration.test.js" "$SB/.held-out/integration.test.js"

cat > "$SEED/package.json" <<'JSON'
{
  "name": "signal-workspace",
  "private": true,
  "version": "0.0.0",
  "description": "A workspace of interdependent packages: @acme/signal (base) + signal-relay + signal-hub.",
  "workspaces": ["signal", "signal-relay", "signal-hub"]
}
JSON
cat > "$SEED/README.md" <<'MD'
# signal-workspace

A workspace of interdependent packages:

- **signal/** — `@acme/signal`, the base product (an in-process named signal bus) + the `signal` CLI.
- **signal-relay/** — relays product signals between hubs (peerDep `@acme/signal`).
- **signal-hub/** — hosts product signals at `signal://` addresses (peerDep `@acme/signal`).
- **config/app.toml** — the product config.

Each package runs its own tests with `node --test`. The consumers resolve the base by a relative shim
(`src/_signal.js`), so no install is required.
MD
cat > "$SEED/.gitignore" <<'GI'
node_modules/
.DS_Store
AGENTS.md
.mcp.json
.codex/
.seed/
GI

echo "== git init seed + bare origin =="
git -C "$SEED" init -q -b main
git -C "$SEED" add -A
# Seed history is fixture data; plumbing keeps agent commit hooks out of immutable setup.
seed_tree="$(git -C "$SEED" write-tree)"
seed_commit="$(
  printf '%s\n' "seed: synthetic signal workspace (base + relay + hub + config)" |
    GIT_AUTHOR_NAME="eval-seed" GIT_AUTHOR_EMAIL="seed@local" \
    GIT_COMMITTER_NAME="eval-seed" GIT_COMMITTER_EMAIL="seed@local" \
    GIT_AUTHOR_DATE="2026-07-30T00:00:00Z" GIT_COMMITTER_DATE="2026-07-30T00:00:00Z" \
    git -C "$SEED" commit-tree "$seed_tree"
)"
git -C "$SEED" update-ref refs/heads/main "$seed_commit"
git clone -q --bare "$SEED" "$SB/origin.git"

echo "== clone one full workspace per agent (distinct authors) + drop its persona =="
for a in sup base relay hub; do
  git clone -q "$SB/origin.git" "$SB/$a"
  git -C "$SB/$a" remote set-url origin "$SB/origin.git"
  git -C "$SB/$a" config user.name  "sig-$a"
  git -C "$SB/$a" config user.email "sig-$a@eval.local"
  # Codex's persona is explicit and inspectable, but gitignored so the agent never commits it.
  cp "$SB/personas/$a.md" "$SB/$a/AGENTS.md"
  test -s "$SB/$a/AGENTS.md"
  mkdir -p "$SB/$a/.codex"
  cp "$SB/harness/codex-hooks.json" "$SB/$a/.codex/hooks.json"
done

echo
echo "SANDBOX READY under $SB: origin.git + sup/ base/ relay/ hub/ (held-out: .held-out/integration.test.js)"
echo "  packages: signal/ (base @acme/signal) · signal-relay/ (primitive trap) · signal-hub/ (signal:// scheme) · config/app.toml"
