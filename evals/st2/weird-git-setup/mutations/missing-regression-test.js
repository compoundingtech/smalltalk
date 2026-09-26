import { test } from "node:test";
import assert from "node:assert/strict";
import { clamp } from "../src/clamp.js";

test("in-range values pass through", () => { assert.equal(clamp(5, 0, 10), 5); });
test("below-range values clamp to lo", () => { assert.equal(clamp(-3, 0, 10), 0); });
