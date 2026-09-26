// PRIMITIVE guard suite — the over-rename tripwire.
//
// Every assertion here is about the RUNTIME primitive, never the product:
// `AbortSignal` / `controller.signal` and the OS signals `SIGTERM` / `SIGINT`.
// If a rename touches the primitive (e.g. a blind s/signal/beacon/g turns
// `controller.signal` into `controller.beacon`, `AbortSignal` into a bad name,
// or `SIGTERM` into `BEACONTERM`), THIS FILE GOES RED. It must stay green
// through a correct PRODUCT-only rename.
import { test } from "node:test";
import assert from "node:assert/strict";

import { Relay } from "../src/relay.js";
import { createSignal } from "../src/_signal.js";

const ADDR = "signal://alpha/greeting";

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

test("controller.signal is a real AbortSignal", () => {
  const controller = new AbortController();
  assert.ok(controller.signal instanceof AbortSignal);
  assert.equal(controller.signal.aborted, false);
});

test("a pre-aborted AbortSignal cancels the relay before it touches the dest", async () => {
  const source = makeHub();
  const dest = makeHub();
  source.hostEnvelope(ADDR, createSignal("greeting", "hi").toEnvelope());
  const relay = new Relay({ source, dest });

  const controller = new AbortController();
  controller.abort(); // fire the primitive up front
  await assert.rejects(
    relay.relaySignal(ADDR, { signal: controller.signal }),
    (err) => err.name === "AbortError",
  );
  assert.equal(dest.has(ADDR), false); // nothing was relayed
});

test("aborting mid-flight rejects the in-flight relay", async () => {
  const source = makeHub();
  const dest = makeHub();
  source.hostEnvelope(ADDR, createSignal("greeting", "hi").toEnvelope());
  const relay = new Relay({ source, dest });

  const controller = new AbortController();
  const pending = relay.relaySignal(ADDR, { signal: controller.signal });
  controller.abort(); // abort while the hop timer is still pending
  await assert.rejects(pending, (err) => err.name === "AbortError");
  assert.equal(dest.has(ADDR), false);
});

test("the cancellation option must be an AbortSignal, not any object", async () => {
  const relay = new Relay({ source: makeHub(), dest: makeHub() });
  await assert.rejects(relay.relaySignal(ADDR, { signal: {} }), TypeError);
});

test("installShutdown wires SIGTERM + SIGINT handlers that cancel in-flight relays", () => {
  const relay = new Relay({ source: makeHub(), dest: makeHub() });
  const beforeTerm = process.listenerCount("SIGTERM");
  const beforeInt = process.listenerCount("SIGINT");

  let shutdownSignal = null;
  const dispose = relay.installShutdown({
    onShutdown: (osSignalName) => (shutdownSignal = osSignalName),
  });

  // A graceful-shutdown hook was installed on BOTH OS signals.
  assert.equal(process.listenerCount("SIGTERM"), beforeTerm + 1);
  assert.equal(process.listenerCount("SIGINT"), beforeInt + 1);

  // An in-flight relay, tracked by its AbortController (primitive).
  const controller = new AbortController();
  relay._inflight.add(controller);
  assert.ok(controller.signal instanceof AbortSignal);
  assert.equal(controller.signal.aborted, false);

  // Synthesize a SIGTERM (does not terminate the test process — a listener is
  // installed, which suppresses the default action).
  process.emit("SIGTERM");

  assert.equal(shutdownSignal, "SIGTERM");
  assert.equal(controller.signal.aborted, true); // the primitive fired

  dispose();
  assert.equal(process.listenerCount("SIGTERM"), beforeTerm);
  assert.equal(process.listenerCount("SIGINT"), beforeInt);
});
