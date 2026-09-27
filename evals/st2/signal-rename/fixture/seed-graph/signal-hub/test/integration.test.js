// CROSS-REPO INTEGRATION suite — the under-rename tripwire.
//
// Wires all three packages together: the base (@acme/signal) through each
// package's shim, the relay (@acme/signal-relay), and the hub (@acme/signal-hub).
// A product signal is hosted at a signal:// address on a source hub, relayed to a
// destination hub, and resolved back to a known value.
//
// This goes RED if the PRODUCT rename is INCOMPLETE or MIS-SEQUENCED across the
// three packages — e.g. the hub's `signal://` scheme is renamed to `beacon://`
// but the relay still only accepts `signal:` (or vice versa). The relay then
// rejects the hub's address and the end-to-end value never resolves. A correct,
// consistent rename (base + relay + hub all moved to `beacon`) keeps it green.
//
// The relay is a sibling package, imported here by relative path (its location,
// `../../signal-relay/...`, is itself a product-rename site).
import { test } from "node:test";
import assert from "node:assert/strict";

import { Hub, address, hostAndResolve } from "../src/hub.js";
import { Relay } from "../../signal-relay/src/relay.js";
import { Signal } from "../src/_signal.js";

const KNOWN = "hello-from-alpha";

test("seam: hostAndResolve wires base+relay+hub and returns the known value", async () => {
  const value = await hostAndResolve({
    Relay,
    host: "alpha",
    topic: "greeting",
    value: KNOWN,
  });
  assert.equal(value, KNOWN);
});

test("manual e2e: host on source, relay to dest, resolve — value + protocol survive", async () => {
  const source = new Hub("source");
  const dest = new Hub("dest");
  const addr = address("alpha", "greeting"); // built from the hub's product scheme

  source.hostSignal(addr, KNOWN, { origin: "integration" });

  const relay = new Relay({ source, dest, name: "seam" });
  await relay.relayTracked(addr); // relay validates the SAME product scheme

  assert.equal(dest.resolveSignal(addr), KNOWN);

  const landed = Signal.fromEnvelope(dest.readEnvelope(addr));
  assert.equal(landed.protocol, "signal/1"); // base protocol tag survived the hop
  assert.equal(landed.meta.origin, "integration");
});

test("the same product scheme is used to host and to relay", async () => {
  // A guard that pins the cross-package agreement the rename must preserve:
  // the hub hosts under `signal://` and the relay accepts that exact scheme.
  const source = new Hub("source");
  const dest = new Hub("dest");
  const addr = address("beta", "status");
  source.hostSignal(addr, { ok: true });

  const relay = new Relay({ source, dest });
  await relay.relayTracked(addr); // would throw "refusing scheme" on a mismatch

  assert.deepEqual(dest.resolveSignal(addr), { ok: true });
});
