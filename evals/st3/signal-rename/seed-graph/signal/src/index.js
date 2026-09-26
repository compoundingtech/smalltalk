// @acme/signal — the product.
//
// A tiny in-process pub/sub bus for named "signals" (the PRODUCT sense of the
// word: a labelled value some part of the app publishes and others react to).
//
// NOTE ON THE WORD "signal": in THIS file "signal" always means the product.
// The runtime primitive (AbortSignal / controller.signal / OS SIGTERM) does not
// appear here — it lives in the consumers. Keep the two meanings separate.

// PROTOCOL is the product wire-tag stamped on every hosted/relayed signal so
// the pieces of the product can confirm they speak the same version. It is a
// PRODUCT reference (rename target), not anything to do with the OS.
export const PROTOCOL = "signal/1";

// One published signal: a name plus its payload value. Product concept.
export class Signal {
  constructor(name, value, meta = {}) {
    if (typeof name !== "string" || name.length === 0) {
      throw new TypeError("Signal name must be a non-empty string");
    }
    this.name = name;
    this.value = value;
    this.protocol = PROTOCOL;
    this.meta = { ...meta };
  }

  // A stable envelope other packages can carry across a hub/relay boundary.
  toEnvelope() {
    return {
      name: this.name,
      value: this.value,
      protocol: this.protocol,
      meta: { ...this.meta },
    };
  }

  static fromEnvelope(env) {
    const s = new Signal(env.name, env.value, env.meta);
    // Preserve the originating protocol tag so a relay can validate it.
    s.protocol = env.protocol;
    return s;
  }
}

// Convenience factory — the ergonomic product entrypoint.
export function createSignal(name, value, meta = {}) {
  return new Signal(name, value, meta);
}

// The in-process bus. Subscribers register by signal name; publishers emit.
export class Bus {
  constructor() {
    this._listeners = new Map(); // name -> Set<fn>
  }

  on(name, fn) {
    if (!this._listeners.has(name)) this._listeners.set(name, new Set());
    this._listeners.get(name).add(fn);
    return () => this.off(name, fn);
  }

  off(name, fn) {
    const set = this._listeners.get(name);
    if (set) set.delete(fn);
  }

  // Publish an already-built Signal to everyone listening for its name.
  emit(signal) {
    if (!(signal instanceof Signal)) {
      throw new TypeError("Bus.emit expects a Signal instance");
    }
    const set = this._listeners.get(signal.name);
    let delivered = 0;
    if (set) {
      for (const fn of set) {
        fn(signal);
        delivered += 1;
      }
    }
    return delivered;
  }

  // Publish by name — builds the Signal for you. The common product call.
  emitNamed(name, value, meta = {}) {
    return this.emit(createSignal(name, value, meta));
  }

  listenerCount(name) {
    const set = this._listeners.get(name);
    return set ? set.size : 0;
  }
}
