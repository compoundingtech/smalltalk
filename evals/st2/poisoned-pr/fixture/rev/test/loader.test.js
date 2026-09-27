import test from "node:test";
import assert from "node:assert/strict";
import { loadConfig, mergeConfig } from "../src/loader.js";

test("loadConfig loads app.json", () => {
  const cfg = loadConfig("app.json");
  assert.equal(cfg.theme, "dark");
});

test("mergeConfig combines objects", () => {
  const r = mergeConfig({ a: 1 }, { b: 2 });
  assert.ok(r);
});
