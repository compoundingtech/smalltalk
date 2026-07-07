// tests/setup/pty-isolation.ts — mandatory PTY_SESSION_DIR isolation.
//
// Runs in every vitest worker BEFORE any test file imports, so any
// `spawn`/`spawnSync('pty', ...)` call inherits a temp PTY_SESSION_DIR
// via process.env and the user's real ~/.local/state/pty/ stays clean.
//
// Background: brief-020 — operator discovered 120 leaked
// `coord-ding-it-*` sessions in their real pty session list, all
// originating from tests/integration/ding.test.ts spawning real pty
// sessions without isolating PTY_SESSION_DIR. A one-off test fix
// would have been hostage to future regressions; this setup makes
// the leak path impossible by construction.
//
// DO NOT remove or bypass this file. See CONTRIBUTING.md.

import { mkdtempSync, realpathSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

// Pick the shortest writable temp area we can.
//
// On macOS, os.tmpdir() is /var/folders/<hash>/T (~56 bytes), which
// combined with pty's <PTY_SESSION_DIR>/<session>.sock layout pushes
// us past the 104-byte unix socket path kernel limit. /tmp (resolving
// to /private/tmp via realpath on macOS, /tmp on Linux) is always
// short and POSIX-canonical.
function pickTempBase(): string {
  try {
    return realpathSync('/tmp');
  } catch {
    return tmpdir();
  }
}
const tempBase = pickTempBase();

// Allocate a per-worker pty session dir BEFORE any test imports.
// Short prefix to keep the resulting <base>/<prefix><suffix>/<session>.sock
// path well under the socket-path limit.
const isolatedDir = mkdtempSync(join(tempBase, 'cpty-'));
process.env.PTY_SESSION_DIR = isolatedDir;

// Hard guard: refuse to run if some other setup clobbered the env or
// pointed it somewhere outside an accepted temp area. The whole point
// is "impossible by construction, not policed by review" — so this
// throws, not warns.
//
// Accepted: the base we used (tempBase), /tmp + its realpath, and
// os.tmpdir() + its realpath. Anything else (the user's HOME, an
// absolute path outside tmp) trips the guard.
function buildAcceptedPrefixes(): readonly string[] {
  const accepted = new Set<string>([tempBase, '/tmp', tmpdir()]);
  for (const p of ['/tmp', tmpdir()]) {
    try {
      accepted.add(realpathSync(p));
    } catch {
      // ignore — only the resolvable entries count
    }
  }
  return [...accepted];
}
const ACCEPTED_PREFIXES = buildAcceptedPrefixes();

if (
  !process.env.PTY_SESSION_DIR ||
  !ACCEPTED_PREFIXES.some((p) => process.env.PTY_SESSION_DIR!.startsWith(p))
) {
  throw new Error(
    `PTY_SESSION_DIR must be inside an OS temp dir for tests; got ` +
      `'${process.env.PTY_SESSION_DIR}'. Accepted prefixes: ` +
      `${ACCEPTED_PREFIXES.join(', ')}. This is a safety check to ` +
      `prevent coord tests from polluting the user's real pty session ` +
      `dir. See tests/setup/pty-isolation.ts.`
  );
}

// Visibility: print one line per worker so a test run shows the
// isolation is active. Per brief-020 task 5, keep for one round
// then remove.
// eslint-disable-next-line no-console
console.log(`[pty isolation] PTY_SESSION_DIR = ${process.env.PTY_SESSION_DIR}`);

// Best-effort cleanup of the temp dir at process exit. Sessions
// inside have already been killed by per-test cleanup; this just
// removes the parent dir.
process.on('exit', () => {
  try {
    rmSync(isolatedDir, { recursive: true, force: true });
  } catch {
    // ignore — process is exiting anyway
  }
});
