#!/usr/bin/env bash
# The single fixtures gate developers and CI run. Proves the synthetic world is deterministic,
# decodes against this repository's canonical client schema, keeps every identity inside the
# synthetic namespace, carries no denylisted content, and that committed reference examples are
# fresh output of the generator. Requires the pinned `nix develop .#web` toolchain and
# `pnpm install --frozen-lockfile` at the repository root for the schema runtime.
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../.." && pwd)"
fixtures="$root/apps/fractal-web/scripts/synthetic-fixtures"
out="$(mktemp -d)"
trap 'rm -rf "$out"' EXIT
node "$fixtures/generate.mjs" --seed 138 --out "$out"
# Generic public rules only: real identities are never committed. Owners additionally run the
# same scanner with --policy and their private identity list before publishing a diff.
node "$fixtures/scan.mjs" "$out" "$root/apps/fractal-web" "$root/scripts/ci-fractal-web"
node --test "$fixtures/privacy.integration.test.mjs"
node "$fixtures/verify.mjs" --fixtures "$out" --native "$root/clients/typescript/st3-client/Schema.generated.ts"
# Committed reference examples must equal fresh generator output exactly: no drift, no extra files.
diff -r "$out" "$fixtures/example" >/dev/null || { echo "fractal-web fixtures gate: example/ is stale; regenerate with generate.mjs --seed 138 --out $fixtures/example" >&2; exit 1; }
echo "fractal-web fixtures gate: PASS"
