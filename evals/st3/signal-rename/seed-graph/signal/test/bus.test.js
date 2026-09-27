// Product API tests for the Signal bus. Exercises the pub/sub product surface.
import { test } from "node:test";
import assert from "node:assert/strict";

import { Bus, Signal, createSignal, PROTOCOL } from "../src/index.js";

test("createSignal builds a product Signal with the protocol tag", () => {
  const sig = createSignal("temperature", 21.5);
  assert.ok(sig instanceof Signal);
  assert.equal(sig.name, "temperature");
  assert.equal(sig.value, 21.5);
  assert.equal(sig.protocol, PROTOCOL);
  assert.equal(PROTOCOL, "signal/1");
});

test("Signal rejects an empty name", () => {
  assert.throws(() => createSignal("", 1), TypeError);
});

test("Bus delivers an emitted signal to subscribers by name", () => {
  const bus = new Bus();
  const seen = [];
  bus.on("greeting", (sig) => seen.push(sig.value));
  const delivered = bus.emitNamed("greeting", "hello");
  assert.equal(delivered, 1);
  assert.deepEqual(seen, ["hello"]);
});

test("Bus only notifies listeners for the matching signal name", () => {
  const bus = new Bus();
  let a = 0;
  let b = 0;
  bus.on("a", () => (a += 1));
  bus.on("b", () => (b += 1));
  bus.emitNamed("a", 1);
  assert.equal(a, 1);
  assert.equal(b, 0);
  assert.equal(bus.listenerCount("a"), 1);
});

test("off / unsubscribe stops delivery", () => {
  const bus = new Bus();
  let count = 0;
  const unsub = bus.on("tick", () => (count += 1));
  bus.emitNamed("tick", 1);
  unsub();
  bus.emitNamed("tick", 2);
  assert.equal(count, 1);
});

test("emit rejects a non-Signal argument", () => {
  const bus = new Bus();
  assert.throws(() => bus.emit({ name: "x", value: 1 }), TypeError);
});

test("envelope round-trips a signal", () => {
  const sig = createSignal("status", { ok: true }, { origin: "test" });
  const back = Signal.fromEnvelope(sig.toEnvelope());
  assert.equal(back.name, "status");
  assert.deepEqual(back.value, { ok: true });
  assert.equal(back.protocol, PROTOCOL);
  assert.equal(back.meta.origin, "test");
});
