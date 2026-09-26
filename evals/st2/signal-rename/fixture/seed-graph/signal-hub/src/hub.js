// @acme/signal-hub — hosts PRODUCT signals at signal:// addresses.
//
// Everything named "signal" in this file is the PRODUCT: the @acme/signal
// payloads we host, and the `signal://` address SCHEME we resolve them under.
// The `signal:` scheme is a product reference (-> `beacon:` on rename). There is
// no runtime primitive here — no AbortSignal, no OS SIGTERM. (Those live in the
// relay.) So a product rename touches this whole file; nothing here must survive
// as "signal".

import { Signal, createSignal } from "./_signal.js"; // the PRODUCT base

// The PRODUCT address scheme this hub speaks (-> "beacon:" on rename). Must stay
// in lock-step with @acme/signal-relay's ACCEPT_SCHEME or the relay will reject
// this hub's addresses — that cross-package agreement is what integration.test
// guards.
export const SCHEME = "signal:";

// Build a product address string: signal://<host>/<topic> (-> beacon://...).
export function address(host, topic) {
  return `${SCHEME}//${host}/${topic}`;
}

// Parse a product address, enforcing the product scheme.
export function parseAddress(addr) {
  const url = new URL(addr);
  if (url.protocol !== SCHEME) {
    throw new Error(`signal-hub: not a ${SCHEME} address: ${addr}`);
  }
  return { host: url.hostname, topic: url.pathname.replace(/^\//, "") };
}

export class Hub {
  constructor(name = "hub") {
    this.name = name;
    this._store = new Map(); // address -> envelope
  }

  // Host a product signal built from a value at a signal:// address.
  hostSignal(addr, value, meta = {}) {
    const { topic } = parseAddress(addr); // validates the product scheme
    const sig = createSignal(topic, value, meta);
    this._store.set(addr, sig.toEnvelope());
    return sig;
  }

  // Host a pre-built product envelope (the seam the relay re-hosts through).
  hostEnvelope(addr, envelope) {
    parseAddress(addr);
    this._store.set(addr, { ...envelope, meta: { ...envelope.meta } });
    return this;
  }

  // Read the raw product envelope (the seam the relay reads from).
  readEnvelope(addr) {
    if (!this._store.has(addr)) {
      throw new Error(`signal-hub: nothing hosted at ${addr}`);
    }
    return this._store.get(addr);
  }

  // Resolve a hosted product signal to its value.
  resolveSignal(addr) {
    return Signal.fromEnvelope(this.readEnvelope(addr)).value;
  }

  has(addr) {
    return this._store.has(addr);
  }
}

// ---------------------------------------------------------------------------
// INTEGRATION SEAM — the held-out grader entrypoint.
//
// hostAndResolve wires base + relay + hub end to end: it hosts a product signal
// at a signal:// address on a source hub, relays it to a destination hub, and
// resolves it back — returning the known value. The `Relay` class is INJECTED so
// @acme/signal-hub needs no hard dependency on @acme/signal-relay.
//
// This whole path only completes if the PRODUCT rename is CONSISTENT across the
// three packages: the hub's SCHEME (`signal://`) must match the relay's accepted
// scheme, and both shims must repoint at the renamed base. An inconsistent or
// mis-sequenced rename makes the relay reject the hub's address and this throws —
// which is exactly how integration.test.js catches an under-done rename.
// ---------------------------------------------------------------------------
export async function hostAndResolve({
  Relay,
  host = "alpha",
  topic = "greeting",
  value,
  meta = {},
} = {}) {
  if (typeof Relay !== "function") {
    throw new TypeError(
      "hostAndResolve: inject the Relay class from @acme/signal-relay",
    );
  }
  const source = new Hub("source");
  const dest = new Hub("dest");
  const addr = address(host, topic); // signal://alpha/greeting

  source.hostSignal(addr, value, meta); // product signal on the source hub
  const relay = new Relay({ source, dest, name: "seam" });
  await relay.relayTracked(addr); // move it across (scheme-checked both ends)

  return dest.resolveSignal(addr); // resolve back to the known value
}
