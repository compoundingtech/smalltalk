// PRODUCT suite for the hub: parse a signal:// address and host/resolve a
// product signal through the base. All refs here are product refs a correct
// rename should touch (the signal:// scheme, hostSignal, resolveSignal).
import { test } from "node:test";
import assert from "node:assert/strict";

import { Hub, SCHEME, address, parseAddress } from "../src/hub.js";
import { Signal } from "../src/_signal.js";

test("address builds a product signal:// address", () => {
  assert.equal(SCHEME, "signal:");
  assert.equal(address("alpha", "greeting"), "signal://alpha/greeting");
});

test("parseAddress accepts the product scheme and extracts host + topic", () => {
  const { host, topic } = parseAddress("signal://alpha/greeting");
  assert.equal(host, "alpha");
  assert.equal(topic, "greeting");
});

test("parseAddress rejects a non-product scheme", () => {
  assert.throws(() => parseAddress("http://alpha/greeting"), /not a signal: address/);
});

test("hosts a product signal at an address and resolves it back", () => {
  const hub = new Hub();
  const addr = address("alpha", "temperature");
  hub.hostSignal(addr, 21.5, { unit: "C" });

  assert.equal(hub.has(addr), true);
  assert.equal(hub.resolveSignal(addr), 21.5);

  const sig = Signal.fromEnvelope(hub.readEnvelope(addr));
  assert.equal(sig.name, "temperature"); // topic became the signal name
  assert.equal(sig.protocol, "signal/1"); // base product protocol tag
  assert.equal(sig.meta.unit, "C");
});

test("resolving a missing address throws", () => {
  const hub = new Hub();
  assert.throws(() => hub.resolveSignal("signal://alpha/nope"), /nothing hosted/);
});
