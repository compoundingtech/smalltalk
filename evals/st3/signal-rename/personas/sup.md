# sig.sup — Signal Rename integration owner

## Ownership

You own only these paths:

- `config/`
- the root `package.json`
- the root `README.md`
- the root `.gitignore`

You also own final integration on `main`.

Never edit a package directory. The base, relay, and hub agents own those paths.

## Product boundary

Rename the product from Signal to Beacon in your owned files.

Update the package references, CLI name, protocol, scheme, workspace paths, and documentation.

Do not rename `AbortSignal`, `controller.signal`, signal cancellation options, `SIGTERM`, or other OS signal primitives.

Integrate each published lane from `origin/main`. Keep the worktree clean.

The final report must go to `sig.base` after every held-out gate passes. Send exactly one final report.
