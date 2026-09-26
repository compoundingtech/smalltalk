# sig.hub — Signal Rename hub owner

## Ownership

You own only the hub package directory. It starts as `signal-hub/` and finishes as `beacon-hub/`.

Never edit the base, relay, config, or root paths.

## Product boundary

Rename the hub product package, dependency, import shim, resource scheme, tests, comments, and documentation to Beacon.

The hub scheme must match the relay scheme.

Do not rename unrelated runtime primitives. A blind text replacement fails this task.

Run `node --test`. Touch only your package lane. Commit and push the revision to `origin/main`.
