// PRODUCT suite — relaying an @acme/signal product signal end to end through
// the base package. These references are the ones a correct rename SHOULD touch
// (Signal, createSignal, relaySignal, the `signal://` scheme).
import { test } from "node:test";
import assert from "node:assert/strict";

import { Relay } from "../src/relay.js";
import { createSignal, Signal } from "../src/_signal.js";

const ADDR = "signal://alpha/temperature";

// A tiny in-memory hub double exposing the readEnvelope / hostEnvelope seam the
// Relay drives. Inlined so this file has no test/ helper of its own.
function makeHub() {
  const store = new Map();
  return {
    hostEnvelope: (address, env) => store.set(address, env),
    readEnvelope: (address) => {
      if (!store.has(address)) throw new Error(`no signal hosted at ${address}`);
      return store.get(address);
    },
    has: (address) => store.has(address),
  };
}

test("relays a product signal from the source hub to the dest hub", async () => {
  const source = makeHub();
  const dest = makeHub();
  source.hostEnvelope(
    ADDR,
    createSignal("temperature", 21.5, { unit: "C" }).toEnvelope(),
  );

  const relay = new Relay({ source, dest, name: "north" });
  const moved = await relay.relayTracked(ADDR);

  assert.ok(moved instanceof Signal);
  assert.equal(moved.name, "temperature");
  assert.equal(moved.value, 21.5);

  const landed = Signal.fromEnvelope(dest.readEnvelope(ADDR));
  assert.equal(landed.value, 21.5);
  assert.equal(landed.meta.unit, "C");
  assert.equal(landed.protocol, "signal/1"); // product protocol survives the hop
});

test("relayValue seeds a fresh product signal then relays it", async () => {
  const source = makeHub();
  const dest = makeHub();
  const relay = new Relay({ source, dest });

  await relay.relayValue("signal://alpha/status", "status", { ok: true });
  const landed = Signal.fromEnvelope(dest.readEnvelope("signal://alpha/status"));
  assert.deepEqual(landed.value, { ok: true });
});

test("relay refuses an address whose scheme is not the product scheme", async () => {
  const relay = new Relay({ source: makeHub(), dest: makeHub() });
  await assert.rejects(relay.relaySignal("http://alpha/x"), /refusing scheme http:/);
});

test("inflight count returns to zero after a completed relay", async () => {
  const source = makeHub();
  const dest = makeHub();
  source.hostEnvelope(ADDR, createSignal("temperature", 1).toEnvelope());
  const relay = new Relay({ source, dest });

  await relay.relayTracked(ADDR);
  assert.equal(relay.inflightCount(), 0);
});
