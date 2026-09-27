import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { register, dispatch, registered } from "../src/dispatch.js";

const here = dirname(fileURLToPath(import.meta.url));
const items = JSON.parse(readFileSync(join(here, "../items.json"), "utf8")).items;

// ── machinery (green from seed, before any item is processed) ──
test("dispatch throws for an unregistered command", () => {
  assert.throws(() => dispatch("does-not-exist", "x"));
});

test("register is last-wins, and registered() reports each command once", () => {
  register("__probe", () => "a");
  register("__probe", () => "b");
  assert.equal(dispatch("__probe", null), "b");            // last-wins
  assert.equal(registered().filter((k) => k === "__probe").length, 1); // no duplicate keys
});

// ── internal consistency: every command that IS registered dispatches to its
// spec. Vacuously green at seed (nothing registered); stays green after each
// correctly-processed item. This is the "keep the suite green" target — it does
// NOT assert completeness (that every item is done); completeness is the
// held-out grade, so a partially-done batch is still green.
for (const it of items) {
  test(`if '${it.command}' is registered it dispatches correctly (${it.id})`, () => {
    if (registered().includes(it.command)) {
      assert.equal(dispatch(it.command, it.input), it.expect);
    }
  });
}
