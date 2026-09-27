# @acme/signal

The **Signal** bus — a tiny, dependency-free, in-process pub/sub for named
**signals** (the product concept: a labelled value one part of an app publishes
and others subscribe to). Ships a `signal` CLI for emitting a test signal or
running a throwaway in-proc signal server.

> The word "signal" in this package always means **the product**. It is not the
> OS signal and not `AbortSignal` — those runtime primitives live in the
> consumers (`@acme/signal-relay`, `@acme/signal-hub`) and are intentionally
> kept separate.

## Install

```sh
npm install @acme/signal
```

## API

```js
import { Bus, Signal, createSignal, PROTOCOL } from "@acme/signal";

const bus = new Bus();
bus.on("temperature", (sig) => console.log(sig.name, sig.value));
bus.emitNamed("temperature", 21.5);          // -> logs: temperature 21.5

const sig = createSignal("temperature", 21.5); // a product Signal
sig.toEnvelope();                               // portable across hub/relay
```

- `PROTOCOL` — the product wire tag (`"signal/1"`) stamped on every signal.
- `class Signal` — one published signal: `{ name, value, protocol, meta }`.
- `createSignal(name, value, meta?)` — factory for a `Signal`.
- `class Bus` — `on(name, fn)`, `off(name, fn)`, `emit(signal)`,
  `emitNamed(name, value, meta?)`, `listenerCount(name)`.

## CLI

```sh
signal emit greeting "hello"     # emit a named product signal and print it
signal serve ready done          # tiny in-proc signal server, emits then exits
signal help
```

## Test

```sh
node --test
```
