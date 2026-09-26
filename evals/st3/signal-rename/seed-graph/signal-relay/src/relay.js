// @acme/signal-relay — relays PRODUCT signals between @acme/signal hubs.
//
// !! READ BEFORE ANY RENAME !!
// This file deliberately mixes the two meanings of the word "signal":
//
//   • PRODUCT signal  — the @acme/signal payload we move between hubs. This is
//     the thing being renamed to `beacon`: the import, `Signal`, `createSignal`,
//     `relaySignal`, the `signal:` scheme, "relay the signal" in docs.
//
//   • PRIMITIVE signal — the runtime `AbortSignal` (`controller.signal`) used to
//     CANCEL an in-flight relay, and the OS signals `SIGTERM` / `SIGINT` used for
//     graceful shutdown. These MUST NOT be renamed. Renaming them breaks
//     cancellation and shutdown, and primitive.test.js goes red.
//
// A blind s/signal/beacon/g corrupts `controller.signal`, `AbortSignal`, the
// `{ signal }` cancellation option, and the SIGTERM handler. Rename by meaning.

import { Signal, createSignal } from "./_signal.js"; // the PRODUCT base

// The PRODUCT address scheme this relay accepts (-> "beacon:" on rename).
export const ACCEPT_SCHEME = "signal:";

export class Relay {
  constructor({ source, dest, name = "relay" } = {}) {
    this.source = source; // a hub exposing readEnvelope(address)
    this.dest = dest; // a hub exposing hostEnvelope(address, envelope)
    this.name = name;
    // Live AbortControllers for in-flight relays, so shutdown can cancel them.
    // (PRIMITIVE — one AbortController per tracked relay.)
    this._inflight = new Set();
    this._shutdownDisposers = [];
  }

  // Relay one PRODUCT signal from the source hub to the dest hub.
  //
  // `opts.signal` is the PRIMITIVE: an optional AbortSignal that cancels the
  // relay in flight. It is NOT the product signal — the product signal is the
  // envelope we read from the source and re-host on the dest.
  async relaySignal(address, { signal } = {}) {
    // The cancellation option, when present, must be a real AbortSignal.
    if (signal && !(signal instanceof AbortSignal)) {
      throw new TypeError("relaySignal: `signal` option must be an AbortSignal");
    }

    // Validate the PRODUCT scheme of the address before doing any work.
    const url = new URL(address);
    if (url.protocol !== ACCEPT_SCHEME) {
      throw new Error(
        `relay ${this.name}: refusing scheme ${url.protocol} (expected ${ACCEPT_SCHEME})`,
      );
    }

    // If the caller already aborted (primitive), stop before touching hubs.
    if (signal?.aborted) throw abortError(signal);

    // Pull the PRODUCT signal envelope out of the source hub and rebuild it
    // through the base package so protocol/meta survive the hop.
    const envelope = await this.source.readEnvelope(address);
    const product = Signal.fromEnvelope(envelope);

    // A cancellable async hop — the AbortSignal (primitive) can interrupt it.
    await this._hop(signal);

    // Re-host the product signal on the destination hub at the same address.
    await this.dest.hostEnvelope(address, product.toEnvelope());
    return product;
  }

  // Mint an AbortController (PRIMITIVE), track it so shutdown can cancel it,
  // and relay the product signal under that controller's `.signal`.
  async relayTracked(address) {
    const controller = new AbortController();
    this._inflight.add(controller);
    try {
      return await this.relaySignal(address, { signal: controller.signal });
    } finally {
      this._inflight.delete(controller);
    }
  }

  // Convenience: seed a product signal on the source hub, then relay it.
  async relayValue(address, name, value, meta = {}) {
    const product = createSignal(name, value, meta);
    await this.source.hostEnvelope(address, product.toEnvelope());
    return this.relayTracked(address);
  }

  // A cancellable async step. Wires the PRIMITIVE AbortSignal to a timer so an
  // abort rejects the in-flight relay.
  _hop(signal) {
    return new Promise((resolve, reject) => {
      if (signal?.aborted) {
        reject(abortError(signal));
        return;
      }
      const timer = setTimeout(resolve, 5);
      if (signal) {
        signal.addEventListener(
          "abort",
          () => {
            clearTimeout(timer);
            reject(abortError(signal));
          },
          { once: true },
        );
      }
    });
  }

  // Install graceful-shutdown handlers on the OS PRIMITIVE signals SIGTERM and
  // SIGINT. On either, cancel every in-flight relay via its AbortController,
  // then invoke onShutdown. Returns a disposer that removes the handlers.
  installShutdown({ onShutdown } = {}) {
    const handler = (osSignalName) => {
      for (const controller of this._inflight) controller.abort();
      if (onShutdown) onShutdown(osSignalName);
    };
    const onTerm = () => handler("SIGTERM");
    const onInt = () => handler("SIGINT");
    process.on("SIGTERM", onTerm);
    process.on("SIGINT", onInt);

    const dispose = () => {
      process.off("SIGTERM", onTerm);
      process.off("SIGINT", onInt);
    };
    this._shutdownDisposers.push(dispose);
    return dispose;
  }

  // Remove any shutdown handlers this relay installed (keeps tests leak-free).
  disposeShutdown() {
    for (const dispose of this._shutdownDisposers) dispose();
    this._shutdownDisposers = [];
  }

  inflightCount() {
    return this._inflight.size;
  }
}

// Build an AbortError from an AbortSignal's reason (primitive plumbing).
function abortError(signal) {
  const reason = signal?.reason;
  if (reason instanceof Error) return reason;
  const err = new Error("relay aborted");
  err.name = "AbortError";
  return err;
}
