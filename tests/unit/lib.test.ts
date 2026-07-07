// tests/unit/lib.test.ts — exercise createSt's wiring.
//
// The deep edge cases are covered by tests/unit/<command>.test.ts. This
// file just verifies the factory routes calls correctly, threads
// `{ root, identity, configRoot }` everywhere, and surfaces typed
// errors as expected.

import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { createSt, type St } from '../../src/lib.ts';
import { asFilename, asIdentity, type Filename, type Identity } from '../../src/types.ts';
import {
  EmptyBodyError,
  IdentityNotHostedError,
  InvalidStateError,
  MessageNotFoundError,
} from '../../src/errors.ts';

let scratch: string;
let stRoot: string;
let alice: Identity;
let bob: Identity;
let coord: St;

beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), 'coord-lib-test-'));
  stRoot = join(scratch, 'coord');
  mkdirSync(stRoot, { recursive: true });
  mkdirSync(join(stRoot, 'alice', 'inbox'), { recursive: true });
  mkdirSync(join(stRoot, 'alice', 'archive'), { recursive: true });
  mkdirSync(join(stRoot, 'bob', 'inbox'), { recursive: true });
  mkdirSync(join(stRoot, 'bob', 'archive'), { recursive: true });
  alice = asIdentity('alice');
  bob = asIdentity('bob');
  coord = createSt({
    root: stRoot,
    identity: alice,
    configRoot: join(scratch, 'config'),
  });
  mkdirSync(join(scratch, 'config'), { recursive: true });
});

afterEach(() => {
  rmSync(scratch, { recursive: true, force: true });
});

// ─── construction ──────────────────────────────────────────────────────

describe('createSt — construction', () => {
  it('exposes root, identity, configRoot', () => {
    expect(coord.root).toBe(stRoot);
    expect(coord.identity).toBe('alice');
    expect(coord.configRoot).toBe(join(scratch, 'config'));
  });

  it('defaults configRoot to ~/.config/smalltalk', () => {
    const c = createSt({ root: stRoot, identity: alice });
    expect(c.configRoot.endsWith('.config/smalltalk')).toBe(true);
  });

  it('throws if identity is invalid (asIdentity catches at construction)', () => {
    expect(() =>
      createSt({
        root: stRoot,
        identity: 'INVALID' as unknown as Identity,
      })
    ).toThrowError(/invalid (agent name|identity)/);
  });
});

// ─── send ──────────────────────────────────────────────────────────────

describe('coord.send', () => {
  it('writes a file and returns its branded Filename', async () => {
    const filename = await coord.send(bob, 'hello bob');
    expect(filename).toMatch(/^[0-9]{13}-[0-9a-z]{6}\.md$/);
    expect(existsSync(join(stRoot, 'bob', 'inbox', filename))).toBe(true);
  });

  it('uses the Coord identity as `from` by default', async () => {
    const filename = await coord.send(bob, 'body');
    const text = require('node:fs').readFileSync(
      join(stRoot, 'bob', 'inbox', filename),
      'utf8'
    );
    expect(text).toContain('from: alice');
  });

  it('opts.from overrides the default identity', async () => {
    const filename = await coord.send(bob, 'body', {
      from: asIdentity('bob'), // bob sending to himself for the test
    });
    const text = require('node:fs').readFileSync(
      join(stRoot, 'bob', 'inbox', filename),
      'utf8'
    );
    expect(text).toContain('from: bob');
  });

  it('throws EmptyBodyError on empty body', async () => {
    await expect(coord.send(bob, '')).rejects.toBeInstanceOf(EmptyBodyError);
  });

  it('subject + tags + priority emit into frontmatter', async () => {
    const filename = await coord.send(bob, 'body', {
      subject: 're: hello',
      tags: ['a', 'b'],
      priority: 'high',
    });
    const text = require('node:fs').readFileSync(
      join(stRoot, 'bob', 'inbox', filename),
      'utf8'
    );
    expect(text).toContain('subject: "re: hello"');
    expect(text).toContain('tags: [a, b]');
    expect(text).toContain('priority: high');
  });
});

// ─── ls ────────────────────────────────────────────────────────────────

describe('coord.ls', () => {
  it('defaults to the Coord identity\'s inbox', async () => {
    await coord.send(alice, 'msg', { from: bob }); // bob → alice
    const matches = await coord.ls();
    expect(matches).toHaveLength(1);
  });

  it('explicit identity arg overrides default', async () => {
    await coord.send(bob, 'msg');
    const matches = await coord.ls(bob);
    expect(matches).toHaveLength(1);
  });

  it('--archive lists archive folder', async () => {
    const f = await coord.send(bob, 'msg');
    await coord.archive(bob, f);
    expect(await coord.ls(bob)).toEqual([]);
    const arch = await coord.ls(bob, { archive: true });
    expect(arch).toEqual([f]);
  });

  it('--since filters by filename ts', async () => {
    const f = await coord.send(bob, 'msg');
    const ts = Number(f.slice(0, 13));
    expect(await coord.ls(bob, { since: ts })).toEqual([f]);
    expect(await coord.ls(bob, { since: ts + 1 })).toEqual([]);
  });

  it('--from filters by frontmatter from key', async () => {
    await coord.send(bob, 'msg from alice'); // from = alice
    expect(await coord.ls(bob, { fromFilter: alice })).toHaveLength(1);
    expect(await coord.ls(bob, { fromFilter: bob })).toEqual([]);
  });
});

// ─── read ──────────────────────────────────────────────────────────────

describe('coord.read', () => {
  it('returns a typed MessageWithLocation', async () => {
    const filename = await coord.send(bob, 'hello bob', { subject: 'hi' });
    const r = await coord.read(bob, filename);
    expect(r.filename).toBe(filename);
    expect(r.identity).toBe('bob');
    expect(r.folder).toBe('inbox');
    expect(r.message.from).toBe('alice');
    expect(r.message.subject).toBe('hi');
    expect(r.message.body).toBe('hello bob\n');
  });

  it('throws MessageNotFoundError for missing file', async () => {
    await expect(
      coord.read(bob, asFilename('1714826789010-zzzzzz.md'))
    ).rejects.toBeInstanceOf(MessageNotFoundError);
  });

  it('falls back to archive automatically', async () => {
    const f = await coord.send(bob, 'msg');
    await coord.archive(bob, f);
    const r = await coord.read(bob, f);
    expect(r.folder).toBe('archive');
  });
});

// ─── archive (+ trim) ──────────────────────────────────────────────────

describe('coord.archive', () => {
  it('moves inbox → archive', async () => {
    const f = await coord.send(bob, 'msg');
    await coord.archive(bob, f);
    expect(existsSync(join(stRoot, 'bob', 'inbox', f))).toBe(false);
    expect(existsSync(join(stRoot, 'bob', 'archive', f))).toBe(true);
  });
});

describe('coord.archiveTrim', () => {
  it('removes files older than the cutoff and returns the victims', async () => {
    // Plant 5 archived files at known timestamps.
    const fakeNow = 1_800_000_000_000;
    const archDir = join(stRoot, 'alice', 'archive');
    for (let i = 0; i < 5; i++) {
      const ts = fakeNow - (i + 1) * 24 * 60 * 60 * 1000;
      writeFileSync(join(archDir, `${ts}-aaaaaa.md`), 'x');
    }
    const victims = await coord.archiveTrim(alice, {
      olderThan: '3d',
      now: () => fakeNow,
    });
    expect(victims).toHaveLength(2); // 2 files older than 3 days
  });

  it('--dry-run returns victims without deleting', async () => {
    const archDir = join(stRoot, 'alice', 'archive');
    for (const ts of ['1714826789010', '1714826789020', '1714826789030']) {
      writeFileSync(join(archDir, `${ts}-aaaaaa.md`), 'x');
    }
    const victims = await coord.archiveTrim(alice, {
      keepLast: 1,
      dryRun: true,
    });
    expect(victims).toHaveLength(2);
    // Files are still on disk.
    expect(require('node:fs').readdirSync(archDir)).toHaveLength(3);
  });
});

// ─── thread ────────────────────────────────────────────────────────────

describe('coord.thread', () => {
  it('returns MessageWithLocation[] in flat chronological order', async () => {
    const f1 = await coord.send(bob, 'root', { subject: 'A' });
    // Force a strictly-later ms so chronological sort = send order
    // (two sends in the same ms break this ordering deterministically).
    await new Promise((r) => setTimeout(r, 2));
    const f2 = await coord.send(bob, 'reply', {
      subject: 'B',
      inReplyTo: f1,
    });
    const out = await coord.thread(bob, f1);
    expect(out.map((m) => m.filename)).toEqual([f1, f2]);
    expect(out[0]!.message.subject).toBe('A');
    expect(out[1]!.message.subject).toBe('B');
  });
});

// ─── status ────────────────────────────────────────────────────────────

describe('coord.getStatus / setStatus', () => {
  it('default is offline', async () => {
    expect(await coord.getStatus(alice)).toBe('offline');
  });

  it('roundtrip', async () => {
    await coord.setStatus(alice, 'busy');
    expect(await coord.getStatus(alice)).toBe('busy');
  });

  it('setStatus with invalid state throws InvalidStateError', async () => {
    await expect(
      coord.setStatus(alice, 'urgent' as unknown as 'busy')
    ).rejects.toBeInstanceOf(InvalidStateError);
  });
});

// ─── sweep ─────────────────────────────────────────────────────────────

describe('coord.sweep', () => {
  it('removes byte-identical inbox+archive twins', async () => {
    const f = '1714826789010-aaaaaa.md';
    writeFileSync(join(stRoot, 'bob', 'inbox', f), 'same');
    writeFileSync(join(stRoot, 'bob', 'archive', f), 'same');
    const r = await coord.sweep();
    expect(r.removed).toBe(1);
  });
});

// ─── identity validation ──────────────────────────────────────────────

describe('coord — identity-not-hosted error path', () => {
  it('throws IdentityNotHostedError when an identity has no folder', async () => {
    const ghost = asIdentity('ghost');
    await expect(coord.archive(ghost, asFilename('1714826789010-aaaaaa.md')))
      .rejects.toBeInstanceOf(IdentityNotHostedError);
  });
});

// ─── watch (async iterable, AbortSignal) ──────────────────────────────

describe('coord.watch', () => {
  it('replay phase yields existing files at cutoff=0 (default)', async () => {
    await coord.send(alice, 'msg from bob', { from: bob });
    const ac = new AbortController();
    const seen: string[] = [];
    const iterable = coord.watch(alice, { intervalMs: 50, signal: ac.signal });
    const consume = (async () => {
      for await (const ev of iterable) {
        seen.push(ev.filename);
        ac.abort(); // stop after first event
      }
    })();
    await consume;
    expect(seen).toHaveLength(1);
  });

  it('AbortSignal stops the iterator', async () => {
    const ac = new AbortController();
    const seen: string[] = [];
    const consume = (async () => {
      for await (const ev of coord.watch(bob, {
        intervalMs: 50,
        signal: ac.signal,
      })) {
        seen.push(ev.filename);
      }
    })();
    // Abort immediately; the iterable should resolve.
    ac.abort();
    await consume;
    expect(seen).toEqual([]);
  });

  it('--all watch suppresses the Coord identity\'s own folder (cross-tree supervisor)', async () => {
    // brief-017a bug 3: cross-tree mode is now opt-in via `{all: true}`.
    // Send to alice (her own inbox) and to bob (peer). With --all,
    // we should see ONLY the bob entry; the watcher's own folder
    // is suppressed.
    await coord.send(alice, 'self-msg', { from: bob });
    await coord.send(bob, 'peer-msg', { from: alice });
    const ac = new AbortController();
    const seen: string[] = [];
    const consume = (async () => {
      for await (const ev of coord.watch(undefined, {
        all: true,
        intervalMs: 50,
        signal: ac.signal,
      })) {
        seen.push(`${ev.identity}:${ev.filename}`);
        ac.abort();
      }
    })();
    await consume;
    expect(seen.every((s) => s.startsWith('bob:'))).toBe(true);
    expect(seen.length).toBeGreaterThan(0);
  });

  it('default watch (no id, no --all) watches the Coord identity\'s own inbox', async () => {
    // brief-017a bug 3: default is now "watch own inbox" — same as
    // CLI `st watch` with no args.
    await coord.send(alice, 'self-msg', { from: bob });
    await coord.send(bob, 'peer-msg', { from: alice });
    const ac = new AbortController();
    const seen: string[] = [];
    const consume = (async () => {
      for await (const ev of coord.watch(undefined, {
        intervalMs: 50,
        signal: ac.signal,
      })) {
        seen.push(`${ev.identity}:${ev.filename}`);
        ac.abort();
      }
    })();
    await consume;
    // The `coord` instance here is built with identity=alice (see the
    // beforeEach). So default watch yields alice:* entries, not bob's.
    expect(seen.every((s) => s.startsWith('alice:'))).toBe(true);
    expect(seen.length).toBeGreaterThan(0);
  });

  it('--with-subject yields the subject', async () => {
    const f = await coord.send(alice, 'msg', {
      from: bob,
      subject: 'hi there',
    });
    void f;
    const ac = new AbortController();
    let captured: string | undefined;
    const consume = (async () => {
      for await (const ev of coord.watch(alice, {
        withSubject: true,
        intervalMs: 50,
        signal: ac.signal,
      })) {
        captured = ev.subject;
        ac.abort();
      }
    })();
    await consume;
    expect(captured).toBe('hi there');
  });
});
