#!/usr/bin/env bash
set -euo pipefail

root="${CATALOG:-$PWD}"
ledger="$root/worker"
test -d "$ledger/.git"

(cd "$ledger" && npm test)
test -z "$(git -C "$ledger" status --porcelain)"

(cd "$ledger" && node --input-type=module <<'NODE'
import { registered } from "./src/dispatch.js";

const commands = registered();
if (commands.length !== 4) throw new Error(`expected 4 commands, found ${commands.length}`);
if (new Set(commands).size !== commands.length) throw new Error("duplicate command registration");
NODE
)

echo "PASS: the tests pass, the worktree is clean, and all command registrations are unique"
