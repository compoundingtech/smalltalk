// examples/tui-watch.ts — minimal interactive consumer of the createSt API.
//
// Run with: npm run example:tui-watch
//
// Picks ST_ROOT and ST_IDENTITY out of the environment, watches the
// caller's own inbox, and renders newly-arriving filenames as they come
// in. Press `a` to archive the most recently received message; press `q`
// (or Ctrl+C) to quit.
//
// To watch every peer's inbox instead (suppressing the caller's own
// folder — the brief-005 cross-tree mode), pass `undefined` to
// `smalltalk.watch()` below. Note: archive on a peer's tree is unusual but
// technically valid (every machine has every tree via sync).
//
// Designed to surface DX issues with the embeddable API. If anything
// here feels clunky, that's a real signal.

import {
  asIdentity,
  type StError,
  createSt,
  type Filename,
  type Identity,
  IdentityNotHostedError,
} from '../src/index.ts';

const root = process.env.ST_ROOT;
const identityRaw = process.env.ST_IDENTITY;
if (!root || !identityRaw) {
  process.stderr.write(
    'usage: ST_ROOT=<path> ST_IDENTITY=<id> npm run example:tui-watch\n'
  );
  process.exit(2);
}

const me: Identity = asIdentity(identityRaw);
const smalltalk = createSt({ root, identity: me });

const ac = new AbortController();
let mostRecent:
  | { filename: Filename; identity: Identity }
  | undefined;

function render(line: string): void {
  process.stdout.write(`${line}\n`);
}

function shutdown(code = 0): void {
  ac.abort();
  if (process.stdin.isTTY) process.stdin.setRawMode(false);
  process.exit(code);
}

process.on('SIGINT', () => shutdown(130));
process.on('SIGTERM', () => shutdown(143));

// Raw-mode keypresses: archive on `a`, quit on `q` / Ctrl+C.
if (process.stdin.isTTY) {
  process.stdin.setRawMode(true);
  process.stdin.resume();
  process.stdin.on('data', async (buf: Buffer) => {
    const ch = buf.toString('utf8');
    if (ch === 'q' || ch === '\x03') {
      shutdown(0);
    } else if (ch === 'a') {
      if (!mostRecent) {
        render('  (no message to archive)');
        return;
      }
      try {
        await smalltalk.archive(mostRecent.identity, mostRecent.filename);
        render(
          `  archived: ${mostRecent.identity}/${mostRecent.filename}`
        );
        mostRecent = undefined;
      } catch (err) {
        const e = err as StError;
        render(
          `  archive failed (${e.code ?? 'UNKNOWN'}): ${e.message}`
        );
      }
    }
  });
}

render(`watching ${me}/inbox — press 'a' to archive most recent, 'q' to quit`);

try {
  // Per-identity watch on the caller's own inbox: the messages here are
  // ones bob/myobie/etc. sent to ME, so "archive most recent" applies.
  for await (const ev of smalltalk.watch(me, {
    withSubject: true,
    intervalMs: 250,
    signal: ac.signal,
  })) {
    mostRecent = { filename: ev.filename, identity: ev.identity };
    const subj = ev.subject ?? '';
    render(`new ${ev.filename}\t${subj}`);
  }
} catch (err) {
  if (err instanceof IdentityNotHostedError) {
    process.stderr.write(`smalltalk: ${err.message}\n`);
    shutdown(1);
  }
  throw err;
}
