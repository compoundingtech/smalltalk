# E2E seams — how the grader drives the renamed stack

This synthetic graph has one deliberate cross-package integration seam. It exists
so a held-out end-to-end check can confirm a **product rename** (`signal` →
`beacon`) was done **completely and consistently** across all three packages —
without the team being able to fake it by editing a single unit test.

## The entrypoint

`@acme/signal-hub` exports the seam:

```js
// signal-hub/src/hub.js
export async function hostAndResolve({ Relay, host = "alpha", topic = "greeting", value, meta = {} })
```

It wires **base + relay + hub** end to end:

1. builds a product address — `signal://<host>/<topic>` (via the hub's `SCHEME`);
2. hosts a product signal carrying `value` on a source `Hub`;
3. relays it to a destination `Hub` using the **injected** `Relay` class
   (the relay validates the address scheme on its side);
4. resolves the signal back from the destination hub and returns its `value`.

The `Relay` class is **injected** (not imported inside the hub) so the hub package
keeps only `@acme/signal` as a peer dependency. The grader supplies it.

### How to call it (pre-rename form)

```js
import { hostAndResolve } from "./signal-hub/src/hub.js";
import { Relay } from "./signal-relay/src/relay.js";

const value = await hostAndResolve({ Relay, host: "alpha", topic: "greeting", value: "known-42" });
// value === "known-42"   ← the end-to-end invariant
```

After a correct rename the same call uses the renamed paths (e.g.
`./beacon-hub/src/hub.js`, `./beacon-relay/src/relay.js`) and still returns the
known value. The in-repo `signal-hub/test/integration.test.js` drives the same
seam via the same relative import.

## What a correct rename MUST keep consistent

The end-to-end value only resolves if **all** of these move together to `beacon`.
Renaming some but not others (an incomplete or mis-sequenced rename) makes the
relay reject the hub's address, or a shim fail to resolve, and the seam throws:

| Consistency point | Pre-rename | Post-rename | Where |
|---|---|---|---|
| base package name | `@acme/signal` | `@acme/beacon` | all `package.json` (`name`, `peerDependencies` key) |
| base package directory | `signal/` | `beacon/` | each shim's relative path |
| CLI bin | `signal` | `beacon` | `signal/package.json` `bin`, `bin/signal.js`, `app.toml` |
| address scheme | `signal://` / `signal:` | `beacon://` / `beacon:` | hub `SCHEME`, relay `ACCEPT_SCHEME`, `app.toml` |
| protocol tag | `signal/1` | `beacon/1` | base `PROTOCOL`, `app.toml` |
| shim path | `../../signal/src/index.js` | `../../beacon/src/index.js` | `signal-relay/src/_signal.js`, `signal-hub/src/_signal.js` |
| relay import path | `../../signal-relay/src/relay.js` | `../../beacon-relay/src/relay.js` | `signal-hub/test/integration.test.js` |
| peerDep key | `"@acme/signal": "*"` | `"@acme/beacon": "*"` | both consumers' `package.json` |

The **hub `SCHEME`** and the **relay `ACCEPT_SCHEME`** are the load-bearing pair:
if one is renamed and the other is not, the relay refuses the address
(`refusing scheme …`) and `integration.test.js` goes red — the under-rename trap.

## What must NOT move (the over-rename trap)

The runtime **primitive** — `AbortSignal` / `controller.signal`, the OS
`SIGTERM` / `SIGINT` handlers, the `{ signal }` cancellation option — lives in
`signal-relay/src/relay.js` and is guarded by `signal-relay/test/primitive.test.js`.
A blind `s/signal/beacon/g` corrupts it and that suite goes red. Rename by
meaning, not by string.
