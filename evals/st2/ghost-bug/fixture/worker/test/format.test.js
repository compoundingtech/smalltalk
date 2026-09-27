import test from "node:test";
import assert from "node:assert/strict";
import { format } from "../src/format.js";

test("default formatting", () => {
  assert.equal(format("hi"), "[ hi ]");
});

test("custom prefix and suffix", () => {
  assert.equal(format("hi", { prefix: "<", suffix: ">" }), "< hi >");
});

test("zero padding (all fields specified)", () => {
  assert.equal(format("hi", { prefix: "[", suffix: "]", pad: 0 }), "[hi]");
});
