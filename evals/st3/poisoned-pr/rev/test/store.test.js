import test from "node:test";
import assert from "node:assert/strict";
import { set, get, clear } from "../src/store.js";
test("set then get", () => { clear(); set("a", 1); assert.equal(get("a"), 1); });
test("missing key is undefined", () => { clear(); assert.equal(get("x"), undefined); });
