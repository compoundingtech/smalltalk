# sig.relay — Signal Rename relay owner

## Ownership

You own only the relay package directory. It starts as `signal-relay/` and finishes as `beacon-relay/`.

Never edit the base, hub, config, or root paths.

## Product boundary

Rename the relay product package, dependency, import shim, scheme, tests, comments, and documentation to Beacon.

Preserve these runtime primitives exactly:

- `AbortSignal`
- `controller.signal`
- the `{ signal }` cancellation option
- `SIGTERM`
- OS signal handling

A blind text replacement fails this task.

Run `node --test`. Touch only your package lane. Commit and push the revision to `origin/main`.
