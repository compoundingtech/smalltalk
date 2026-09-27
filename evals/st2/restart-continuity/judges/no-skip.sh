#!/usr/bin/env bash
set -euo pipefail

root="${CATALOG:-$PWD}"
ledger="$root/worker"
test -d "$ledger/.git"

(cd "$ledger" && node --input-type=module <<'NODE'
import { readFileSync } from "node:fs";
import { dispatch, registered } from "./src/dispatch.js";

const items = JSON.parse(readFileSync("items.json", "utf8")).items;
const commands = new Set(registered());
for (const item of items) {
  if (!commands.has(item.command)) throw new Error(`missing command ${item.command}`);
  if (dispatch(item.command, item.input) !== item.expect) {
    throw new Error(`wrong result for ${item.id}`);
  }
}
NODE
)

while read -r item_id; do
  count="$(grep -cE "^done: $item_id( |$)" "$ledger/PROGRESS.md" || true)"
  test "$count" -ge 1
done < <(cd "$ledger" && node --input-type=module -e \
  'import{readFileSync}from"node:fs";for(const item of JSON.parse(readFileSync("items.json","utf8")).items)console.log(item.id)')

echo "PASS: each stable item has a progress record and a working handler"
