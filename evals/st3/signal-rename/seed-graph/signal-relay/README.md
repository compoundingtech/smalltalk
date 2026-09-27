# @acme/signal-relay

Relays **product signals** between `@acme/signal` hubs. Given a source hub and a
destination hub, `signal-relay` reads a hosted signal and re-hosts it on the
destination — "relay the signal from one hub to the next" — with two runtime
guarantees:

- **cancellation** — an in-flight relay can be cancelled with an `AbortSignal`
  (`controller.signal`);
- **graceful shutdown** — on `SIGTERM` / `SIGINT` the relay cancels every
  in-flight transfer before the process exits.

> ### Two meanings of "signal" — do not conflate them
> - The **product signal** (`@acme/signal`, `Signal`, `createSignal`, the
>   `signal://` address scheme) is what this package moves. That is the product.
> - The **primitive signal** (`AbortSignal` / `controller.signal`, the OS
>   `SIGTERM` / `SIGINT`) is Node runtime machinery for cancellation and
>   shutdown. It is NOT the product and must never be renamed with it.

## Install

```sh
npm install @acme/signal-relay
# peerDependency:
npm install @acme/signal
```

## API

```js
import { Relay } from "@acme/signal-relay";

const relay = new Relay({ source: hubA, dest: hubB, name: "north" });

// cancellable relay of a product signal
const controller = new AbortController();          // primitive
await relay.relaySignal("signal://alpha/temp", { signal: controller.signal });

// graceful shutdown on OS signals
const dispose = relay.installShutdown({ onShutdown: (s) => console.log("bye", s) });
```

- `ACCEPT_SCHEME` — the product address scheme the relay accepts (`"signal:"`).
- `relaySignal(address, { signal? })` — relay one product signal; `signal` is an
  optional **AbortSignal** (primitive) to cancel it.
- `relayTracked(address)` — relay under a fresh tracked `AbortController`.
- `relayValue(address, name, value, meta?)` — seed a product signal, then relay.
- `installShutdown({ onShutdown? })` — install SIGTERM/SIGINT handlers; returns a
  disposer.

## Test

```sh
node --test
```

`test/primitive.test.js` guards the primitive (AbortSignal/SIGTERM); it goes red
if a rename corrupts it. `test/product.test.js` exercises relaying a product
signal end to end through the base.
