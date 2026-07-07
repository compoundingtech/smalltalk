// tests/integration/pi-extension-live.test.ts — end-to-end pi
// extension demo. Spawns a real pi-coding-agent session pointed at
// examples/pi/coord.ts (auto-loaded from a per-test ~/.pi/agent/),
// drops a coord message into the agent's inbox, and waits for the
// agent to call `st_msg_reply` so a reply file lands in the sender's
// inbox.
//
// Skip-gated by `COORD_RUN_LIVE_PI=1`. CI without `pi` on $PATH or
// without the @myobie/coord package resolvable from the per-test
// extensions dir does not run this. The shape proof is what matters;
// we are not running this on every commit.
//
// Manual run:
//   COORD_RUN_LIVE_PI=1 npx vitest run tests/integration/pi-extension-live.test.ts
//
// Requires:
//   - `pi` on $PATH (`npm install -g @mariozechner/pi-coding-agent` works).
//   - The repo's bin/coord on $PATH so the extension can resolve coord.
//   - Network reachable for pi's first-run model registration if the host
//     hasn't run pi before in this $HOME.
//
// The test uses the developer's real $HOME for auth (pi's provider
// credentials live in macOS Keychain / per-user files that don't
// survive HOME isolation), and isolates the extension via
// `pi -ne -e <scratch-path>` so the developer's real
// ~/.pi/agent/extensions/ is untouched. The extension's bare imports
// resolve via a hand-crafted node_modules tree under <scratch> with
// symlinks to the repo root (which IS the @myobie/coord package).

import {
  copyFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { Session } from '@myobie/pty/testing';

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = resolve(HERE, '..', '..');
const COORD_BIN_DIR = join(REPO_ROOT, 'bin');
const EXTENSION_SRC = join(REPO_ROOT, 'examples', 'pi', 'coord.ts');

const LIVE = process.env.COORD_RUN_LIVE_PI === '1';

let scratch: string;
let coordRoot: string;
let extDir: string;
let ptySessionDir: string;
let piSessionsDirBefore: Set<string>;
let session: Session | undefined;

function cleanupSession(): void {
  if (session) {
    try {
      session.close();
    } catch {
      // best-effort
    }
    session = undefined;
  }
}

beforeEach(() => {
  if (!LIVE) return;
  if (!process.env.HOME) {
    throw new Error(
      'pi-extension-live: $HOME must be set (pi auth is HOME-gated)'
    );
  }

  // Per-test PTY_SESSION_DIR so two test files running in parallel
  // can't collide on `/tmp/p`. Path stays short to dodge macOS' 104-
  // byte unix-socket limit.
  ptySessionDir =
    process.env.PTY_SESSION_DIR ??
    mkdtempSync(join(tmpdir(), 'p-'));
  mkdirSync(ptySessionDir, { recursive: true });

  scratch = mkdtempSync(join(tmpdir(), 'coord-pi-it-'));
  coordRoot = join(scratch, 'coord');
  for (const id of ['alice', 'bob']) {
    mkdirSync(join(coordRoot, id, 'inbox'), { recursive: true });
    mkdirSync(join(coordRoot, id, 'archive'), { recursive: true });
  }

  // The extension lives under scratch (not in $HOME/.pi/...) — we
  // load it explicitly via `pi -e <path>` so we don't disturb the
  // developer's real pi extension directory. Use the real $HOME for
  // auth: pi reads provider credentials from keychain / settings.json,
  // both of which only work in the developer's actual home.
  extDir = join(scratch, 'extensions');
  mkdirSync(extDir, { recursive: true });
  copyFileSync(EXTENSION_SRC, join(extDir, 'coord.ts'));

  // package.json + node_modules tree so pi's jiti loader can resolve
  // the extension's bare imports. @myobie/coord points at the repo
  // root (this IS the coord package); typebox is the only other
  // runtime dep; @mariozechner/* is type-only but we symlink the
  // whole namespace as cheap insurance against jiti's transform
  // refusing to strip eagerly.
  //
  // symlinkSync (not spawnSync('ln', ...)): the shell command swallows
  // failures, so a missing typebox or other resolution issue would
  // surface 30s later as a watcher-load timeout with no diagnostic.
  // symlinkSync throws, surfacing the real error.
  writeFileSync(
    join(extDir, 'package.json'),
    JSON.stringify({
      private: true,
      type: 'module',
      dependencies: { '@myobie/coord': 'file:' + REPO_ROOT },
    })
  );
  const nm = join(extDir, 'node_modules');
  mkdirSync(join(nm, '@myobie'), { recursive: true });
  mkdirSync(join(nm, '@mariozechner'), { recursive: true });
  symlinkSync(REPO_ROOT, join(nm, '@myobie', 'coord'));
  symlinkSync(
    join(REPO_ROOT, 'node_modules', 'typebox'),
    join(nm, 'typebox')
  );
  const mzDir = join(REPO_ROOT, 'node_modules', '@mariozechner');
  if (existsSync(mzDir)) {
    for (const name of readdirSync(mzDir)) {
      symlinkSync(join(mzDir, name), join(nm, '@mariozechner', name));
    }
  }

  // Snapshot the pi sessions directory listing so afterEach can
  // delete only test-created entries. Pi writes to
  // ~/.pi/agent/sessions/<encoded-cwd>/ on every spawn; without
  // cleanup the test leaves an orphan dir every run.
  const piSessions = join(process.env.HOME, '.pi', 'agent', 'sessions');
  piSessionsDirBefore = new Set<string>(
    existsSync(piSessions) ? readdirSync(piSessions) : []
  );
});

afterEach(() => {
  cleanupSession();
  if (!LIVE) return;
  // Remove pi session dirs that didn't exist before this test ran.
  try {
    const piSessions = join(process.env.HOME!, '.pi', 'agent', 'sessions');
    if (existsSync(piSessions)) {
      for (const name of readdirSync(piSessions)) {
        if (!piSessionsDirBefore.has(name)) {
          rmSync(join(piSessions, name), { recursive: true, force: true });
        }
      }
    }
  } catch {
    // best-effort
  }
  rmSync(scratch, { recursive: true, force: true });
});

function parseFm(text: string): { fm: string; body: string } | null {
  const m =
    /^---\r?\n([\s\S]*?)\r?\n---\r?\n([\s\S]*)$/.exec(text);
  if (!m || m[1] === undefined || m[2] === undefined) return null;
  return { fm: m[1], body: m[2] };
}

async function pollForReply(
  recipient: string,
  predicate: (filename: string, parsed: { fm: string; body: string }) => boolean,
  timeoutMs: number
): Promise<{ filename: string; fm: string; body: string }> {
  const dir = join(coordRoot, recipient, 'inbox');
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (existsSync(dir)) {
      for (const name of readdirSync(dir)) {
        let text: string;
        try {
          text = readFileSync(join(dir, name), 'utf8');
        } catch {
          // Partial write or transient permissions — skip + retry.
          continue;
        }
        const parsed = parseFm(text);
        if (parsed === null) continue;
        if (predicate(name, parsed)) {
          return { filename: name, fm: parsed.fm, body: parsed.body };
        }
      }
    }
    await new Promise((r) => setTimeout(r, 250));
  }
  throw new Error(
    `pollForReply: no matching file in ${dir} within ${timeoutMs}ms`
  );
}

describe.skipIf(!LIVE)('pi extension — live agent end-to-end', () => {
  it(
    'agent is notified of new coord arrivals and can call st_msg_reply',
    async () => {
      const path = `${COORD_BIN_DIR}:${process.env.PATH ?? ''}`;
      // Pass through whichever provider API key the developer has set.
      // pi can ALSO use keychain-based OAuth — that works because we
      // use the real $HOME below (auth files / keychain ACLs are tied
      // to $HOME / login user, not isolatable).
      const providerEnv: Record<string, string> = {};
      for (const k of [
        'ANTHROPIC_API_KEY',
        'OPENAI_API_KEY',
        'GROQ_API_KEY',
        'GOOGLE_API_KEY',
      ]) {
        const v = process.env[k];
        if (v !== undefined && v.length > 0) providerEnv[k] = v;
      }
      // `-ne` disables extension auto-discovery; `-e <path>` loads our
      // test extension explicitly. Together: pi runs with exactly one
      // extension (ours), and the developer's ~/.pi/agent/extensions/
      // is untouched.
      session = Session.spawn(
        'pi',
        ['-ne', '-e', join(extDir, 'coord.ts')],
        {
          env: {
            HOME: process.env.HOME!,
            PATH: path,
            COORD_ROOT: coordRoot,
            COORD_IDENTITY: 'bob',
            PTY_SESSION_DIR: ptySessionDir,
            ...providerEnv,
          },
        }
      );
      // The extension's session_start notify text is the most reliable
      // "watcher is up" marker.
      await session.waitForText('coord: watching bob/inbox/', 30_000);

      // Drop a question into bob's inbox from alice. The seed prompts
      // the agent to call `st_msg_send` with `inReplyTo` rather than
      // `st_msg_reply`, because pi's extension registers the five
      // Phase-1 verbs (send/ls/read/archive/thread) but NOT
      // st_msg_reply (that's MCP channel-mode only).
      const original = '1714826789010-aaaaaa.md';
      writeFileSync(
        join(coordRoot, 'bob', 'inbox', original),
        '---\nfrom: alice\nsubject: math\n---\n' +
          'what is 2+2? Use the st_msg_send tool with to=alice, ' +
          `inReplyTo=${original}, and your answer as the body.\n`
      );

      // The extension fires `coord: new in bob/inbox` on each arrival
      // before injecting via pi.sendUserMessage; missing the toast is
      // fine (some pi UI modes route notify() to a transient surface
      // we may not capture), but waiting for it speeds up the happy
      // path.
      await session.waitForText(
        'coord: new in bob/inbox',
        15_000
      ).catch(() => {
        // best-effort marker
      });

      // Wait for a reply file in alice/inbox/ that's (a) different
      // from the one we wrote, (b) `from: bob` anchored to a
      // frontmatter line start, (c) `in-reply-to: <original>` anchored
      // similarly, (d) body mentions "4" as a standalone token.
      const fromRe = /^from: bob$/m;
      const replyRe = new RegExp(`^in-reply-to: ${original}$`, 'm');
      const answerRe = /\b4\b/;
      let found: { filename: string; fm: string; body: string };
      try {
        found = await pollForReply(
          'alice',
          (filename, parsed) => {
            if (filename === original) return false;
            if (!fromRe.test(parsed.fm)) return false;
            if (!replyRe.test(parsed.fm)) return false;
            return answerRe.test(parsed.body);
          },
          120_000
        );
      } catch (err) {
        // eslint-disable-next-line no-console
        console.error(
          '[pi-live-test] poll failed; final session screen:\n' +
            session?.screenshot().text
        );
        throw err;
      }
      expect(found.fm).toMatch(fromRe);
      expect(found.fm).toMatch(replyRe);
      expect(found.body).toMatch(answerRe);
    },
    240_000
  );
});
