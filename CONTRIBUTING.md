# Contributing

## Writing tests that touch pty

Any test that spawns `pty` via `spawn`/`spawnSync` MUST run under the global `PTY_SESSION_DIR` isolation set by [`tests/setup/pty-isolation.ts`](tests/setup/pty-isolation.ts). The setup file rewrites `process.env.PTY_SESSION_DIR` to a temp dir before any test file imports, and a hard guard throws if the env var doesn't end up inside an accepted temp area (`/tmp`, `os.tmpdir()`, or their realpaths).

The failure mode is "refuse to run", not "warn" — preventing pollution of the user's real `~/.local/state/pty/` is non-negotiable.

**DO**

- Let `spawn`/`spawnSync('pty', ...)` calls inherit env from `process.env` (omit the `env:` option entirely, or spread `...process.env` first).
- Add a `beforeEach` fast-fail assertion in any new test that spawns `pty`:

  ```ts
  if (!process.env.PTY_SESSION_DIR) {
    throw new Error(
      'PTY_SESSION_DIR is not set — tests/setup/pty-isolation.ts ' +
        'did not run. Refusing to spawn pty sessions into the ' +
        "user's real session dir."
    );
  }
  ```

- Clean up sessions in `afterEach` with `pty kill` + (best-effort) `pty rm`. This is courtesy — the global isolation is the floor. Even if cleanup fails mid-test (interruption, crash), nothing leaks into the user's real dir.

**DO NOT**

- Remove or bypass [`tests/setup/pty-isolation.ts`](tests/setup/pty-isolation.ts), or unregister it from the vitest workspace config.
- Spawn `pty` with an explicit `env:` block that omits `PTY_SESSION_DIR` or hardcodes a path outside the OS temp area.
- Rely on per-test cleanup as the only guard. Test interruptions happen; the global setup is the load-bearing safety net.

If a single test needs a clean session dir per case (rare), `mkdtempSync(tmpdir(), 'smalltalk-pty-')` and override `PTY_SESSION_DIR` for that one spawn, then `rmSync` the dir in `afterEach`. Do NOT mutate `process.env.PTY_SESSION_DIR` itself — only override per-spawn.

### Manual smoke check

Before merging changes that touch a pty-spawning test, run `pty list` from a separate shell during or just after `npm test`, and confirm no new `st-*` / `smalltalk-*` / `st-*` sessions appear in the user's real session dir.

### Background — why this exists

A previous round of `tests/integration/ding.test.ts` leaked 120 `st-ding-it-*` sessions into the user's real `pty list` output over a few weeks (back when the project was still named `smalltalk` — the historical prefix is preserved here for searchability). Per-test cleanup wasn't enough — interrupted runs (crash, Ctrl-C, timeout) left orphans. The global setup makes the leak path impossible by construction.
