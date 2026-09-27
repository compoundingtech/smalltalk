# @acme/signal-hub

Hosts **product signals** from `@acme/signal` at `signal://host/topic` addresses
and resolves them back. The hub is where a product signal "lives" so a relay can
move it and a consumer can look it up.

> Every "signal" in this package is the **product** — including the `signal://`
> address scheme. There is no runtime primitive here; a product rename touches
> the whole package (`@acme/signal` → `@acme/beacon`, `signal://` → `beacon://`).

## Install

```sh
npm install @acme/signal-hub
# peerDependency:
npm install @acme/signal
```

## API

```js
import { Hub, address, hostAndResolve } from "@acme/signal-hub";

const hub = new Hub();
const addr = address("alpha", "temperature");  // "signal://alpha/temperature"
hub.hostSignal(addr, 21.5, { unit: "C" });
hub.resolveSignal(addr);                         // 21.5
```

- `SCHEME` — the product address scheme (`"signal:"`).
- `address(host, topic)` — build a `signal://host/topic` address.
- `parseAddress(addr)` — validate the scheme, return `{ host, topic }`.
- `class Hub` — `hostSignal`, `hostEnvelope`, `readEnvelope`, `resolveSignal`,
  `has`.
- `hostAndResolve({ Relay, host?, topic?, value })` — the **integration seam**:
  hosts a signal, relays it across (Relay injected from `@acme/signal-relay`),
  and resolves the known value. See `../E2E-SEAMS.md`.

## Test

```sh
node --test
```

`test/hub.test.js` covers the product surface. `test/integration.test.js` wires
base + relay + hub end to end and goes red on an inconsistent product rename.
