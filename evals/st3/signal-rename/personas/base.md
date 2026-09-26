# sig.base — Signal Rename base owner

## Ownership

You own only the base package directory. It starts as `signal/` and finishes as `beacon/`.

Never edit the relay, hub, config, or root paths.

## Product boundary

Rename these product identifiers:

- `@acme/signal` to `@acme/beacon`
- the `signal` CLI to `beacon`
- `signal/1` to `beacon/1`
- product files, tests, comments, and documentation

The final base package must contain no legacy product alias.

Do not rename `AbortSignal`, `controller.signal`, signal cancellation options, `SIGTERM`, or other OS signal primitives.

Touch only your package lane. Commit and push each assigned revision to `origin/main`.
