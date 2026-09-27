// Product CLI tests — drive runCli directly and capture its output.
import { test } from "node:test";
import assert from "node:assert/strict";

import { runCli } from "../src/cli.js";

function capture(argv) {
  let out = "";
  const code = runCli(argv, { write: (s) => (out += s) });
  return { code, out };
}

test("emit prints the named product signal", () => {
  const { code, out } = capture(["emit", "greeting", "hello"]);
  assert.equal(code, 0);
  assert.match(out, /signal greeting \[signal\/1\] = "hello"/);
});

test("emit without a name is a usage error", () => {
  const { code } = capture(["emit"]);
  assert.equal(code, 2);
});

test("serve emits the requested signals then exits cleanly", () => {
  const { code, out } = capture(["serve", "ready", "done"]);
  assert.equal(code, 0);
  assert.match(out, /signal server up/);
  assert.match(out, /emitted signal: ready/);
  assert.match(out, /emitted signal: done/);
  assert.match(out, /signal server done \(2 signals\)/);
});

test("help / no args prints usage", () => {
  assert.equal(capture(["help"]).code, 0);
  assert.match(capture([]).out, /the in-process signal bus/);
});
