// tests/unit/lib-coord-api.test.ts — brief-028 Coord handle additions.
//
// Covers coord.members(), coord.overview(), and coord.createIdentity().
// Existing tests/unit/members.test.ts and overview.test.ts exhaustively
// cover the underlying computation; this file just verifies the handle
// wiring + the createIdentity contract.

import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { createSt, type St } from '../../src/lib.ts';
import { InvalidIdentityError } from '../../src/errors.ts';
import { asFilename, asIdentity, type Identity } from '../../src/types.ts';
import type {
  MemberSummary,
  MemberSummaryEnriched,
} from '../../src/commands/agents.ts';
import type { Overview } from '../../src/commands/overview.ts';

let scratch: string;
let stRoot: string;
let coord: St;

function setupIdentity(id: string): void {
  mkdirSync(join(stRoot, id, 'inbox'), { recursive: true });
  mkdirSync(join(stRoot, id, 'archive'), { recursive: true });
}

function setStatus(id: string, value: string): void {
  writeFileSync(join(stRoot, id, 'status'), `${value}\n`);
}

beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), 'coord-lib-api-'));
  stRoot = join(scratch, 'coord');
  mkdirSync(stRoot, { recursive: true });
  setupIdentity('alice');
  setupIdentity('bob');
  setupIdentity('carol');
  coord = createSt({
    root: stRoot,
    identity: asIdentity('alice'),
    configRoot: join(scratch, 'config'),
  });
});

afterEach(() => {
  rmSync(scratch, { recursive: true, force: true });
});

// ─── coord.members ──────────────────────────────────────────────────────

describe('coord.members', () => {
  it('zero-arg returns all identities as MemberSummary[]', () => {
    const r = coord.members() as MemberSummary[];
    expect(r.map((m) => m.identity).sort()).toEqual([
      'alice',
      'bob',
      'carol',
    ]);
    for (const m of r) {
      expect(m).toHaveProperty('identity');
      expect(m).toHaveProperty('status');
      expect(m).toHaveProperty('name');
      // Non-enriched shape: no extras.
      expect(m).not.toHaveProperty('lastActivity');
      expect(m).not.toHaveProperty('inbox');
    }
  });

  it('enrich: true returns MemberSummaryEnriched[]', () => {
    const r = coord.members({ enrich: true }) as MemberSummaryEnriched[];
    expect(r.length).toBeGreaterThan(0);
    for (const m of r) {
      expect(m).toHaveProperty('lastActivity');
      expect(m).toHaveProperty('inbox');
    }
  });

  it('status filter narrows to a single state', () => {
    setStatus('alice', 'available');
    setStatus('bob', 'busy');
    // carol leaves status unset → effective offline
    const r = coord.members({ status: 'busy' });
    expect(r.map((m) => m.identity)).toEqual(['bob']);
  });

  it('returns [] when no identities have hosting folders', () => {
    rmSync(stRoot, { recursive: true, force: true });
    mkdirSync(stRoot, { recursive: true });
    expect(coord.members()).toEqual([]);
  });
});

// ─── coord.overview ─────────────────────────────────────────────────────

describe('coord.overview', () => {
  it('defaults identity to the handle\'s own', () => {
    const r: Overview = coord.overview();
    expect(r.identity).toBe('alice');
    expect(r).toHaveProperty('inbox');
    expect(r).toHaveProperty('members');
    expect(r).toHaveProperty('recent');
  });

  it('opts.identity overrides the default', () => {
    const r: Overview = coord.overview({
      identity: asIdentity('bob'),
    });
    expect(r.identity).toBe('bob');
  });

  it('recent activity defaults to a sensible count and respects opts.recent', () => {
    // Plant a handful of inbox files to populate recent activity.
    for (let i = 0; i < 7; i++) {
      const filename = `${1714826789000 + i * 10}-aaaaaa.md`;
      // Use varied 6-char crockford suffixes to keep filenames distinct.
      const suffix = ['aaaaaa', 'bbbbbb', 'cccccc', 'dddddd', 'eeeeee', 'ffffff', 'gggggg'][i]!;
      const actual = `${1714826789000 + i * 10}-${suffix}.md`;
      writeFileSync(
        join(stRoot, 'alice', 'inbox', actual),
        `---\nfrom: bob\n---\nm${i}\n`
      );
      // suppress unused-var warning from the unused `filename` above
      void filename;
    }
    const full = coord.overview();
    const limited = coord.overview({ recent: 3 });
    expect(limited.recent.length).toBeLessThanOrEqual(3);
    expect(full.recent.length).toBeGreaterThanOrEqual(limited.recent.length);
  });

  it('includes a members section enriched with lastActivity', () => {
    const r = coord.overview();
    expect(r.members.length).toBeGreaterThan(0);
    expect(r.members[0]).toHaveProperty('lastActivity');
  });
});

// ─── coord.createIdentity ───────────────────────────────────────────────

describe('coord.createIdentity', () => {
  it('new name → { created: true } and both folders exist', async () => {
    const r = await coord.createIdentity('dave');
    expect(r).toEqual({ created: true });
    expect(existsSync(join(stRoot, 'dave', 'inbox'))).toBe(true);
    expect(existsSync(join(stRoot, 'dave', 'archive'))).toBe(true);
  });

  it('existing name → { created: false } (idempotent)', async () => {
    // alice was set up in beforeEach with both folders.
    const r = await coord.createIdentity('alice');
    expect(r).toEqual({ created: false });
  });

  it('partial state (only inbox exists) → completes the layout, returns { created: true }', async () => {
    // A sender may have lazily mkdir'd `eve/inbox/` via st_msg_send;
    // archive is missing. createIdentity should backfill.
    mkdirSync(join(stRoot, 'eve', 'inbox'), { recursive: true });
    const r = await coord.createIdentity('eve');
    expect(r).toEqual({ created: true });
    expect(existsSync(join(stRoot, 'eve', 'archive'))).toBe(true);
  });

  it('rejects an invalid identity grammar', async () => {
    await expect(coord.createIdentity('INVALID')).rejects.toThrow(
      InvalidIdentityError
    );
  });

  it('rejects a reserved name (e.g. `members`)', async () => {
    // RESERVED_NAMES includes folder/sidecar names + state words +
    // verb names — validIdentity guards them. Sample one.
    await expect(coord.createIdentity('members')).rejects.toThrow(
      InvalidIdentityError
    );
  });

  it('rejects the brief-022 derived state `unknown` as an identity', async () => {
    // Regression for the brief-024 carry-over reserving `unknown`.
    await expect(coord.createIdentity('unknown')).rejects.toThrow(
      InvalidIdentityError
    );
  });

  it('does NOT set status', async () => {
    await coord.createIdentity('frank');
    expect(existsSync(join(stRoot, 'frank', 'status'))).toBe(false);
  });
});

// ─── public surface (RESERVED_NAMES) ────────────────────────────────────

describe('public surface (brief-028)', () => {
  it('RESERVED_NAMES is exported from @myobie/coord and includes the canonical names', async () => {
    const mod = (await import('../../src/index.ts')) as {
      RESERVED_NAMES: readonly string[];
    };
    for (const name of [
      'inbox',
      'archive',
      'resources',
      'status',
      'unknown',
      'available',
    ]) {
      expect(mod.RESERVED_NAMES).toContain(name);
    }
  });
});

// ─── coord.resources (brief-009 item 5) ────────────────────────────────

describe('coord.resources', () => {
  it('add returns a Filename, list surfaces it back', async () => {
    const fn = await coord.resources.add({
      url: 'https://example.com',
    });
    expect(/^[0-9]{13}-[0-9a-z]{6}\.md$/.test(fn)).toBe(true);
    const items = await coord.resources.list();
    expect(items).toHaveLength(1);
    expect(items[0]!.filename).toBe(fn);
    expect(items[0]!.identity).toBe('alice');
    expect(items[0]!.resource.url).toBe('https://example.com');
  });

  it('list defaults to the handle\'s own identity; explicit arg switches to a peer', async () => {
    await coord.resources.add({ url: 'https://alice.example/' });
    const ownItems = await coord.resources.list();
    expect(ownItems).toHaveLength(1);
    const bobItems = await coord.resources.list(asIdentity('bob'));
    expect(bobItems).toHaveLength(0);
  });

  it('read returns the parsed Resource with optional fields populated', async () => {
    const fn = await coord.resources.add({
      url: 'https://example.com',
      title: 'eg',
      tags: ['a', 'b'],
      relation: 'owns',
      body: 'desc\n',
    });
    const r = await coord.resources.read(asIdentity('alice'), fn);
    expect(r.url).toBe('https://example.com');
    expect(r.title).toBe('eg');
    expect(r.tags).toEqual(['a', 'b']);
    expect(r.relation).toBe('owns');
    expect(r.body).toContain('desc');
  });

  it('relation is absent on the returned Resource when not set on add', async () => {
    const fn = await coord.resources.add({
      url: 'https://example.com',
    });
    const r = await coord.resources.read(asIdentity('alice'), fn);
    expect(r.relation).toBeUndefined();
  });

  it('remove deletes the file; subsequent list shows it gone', async () => {
    const fn = await coord.resources.add({
      url: 'https://example.com',
    });
    await coord.resources.remove(fn);
    const items = await coord.resources.list();
    expect(items).toHaveLength(0);
  });

  it('add rejects URLs without a scheme', async () => {
    await expect(
      coord.resources.add({ url: 'example.com' })
    ).rejects.toThrow();
  });
});

// ─── coord.archive opts (brief-009 item 4 SDK parity) ──────────────────

describe('coord.archive opts.withAttachments', () => {
  it('without opts, the canonical .md moves and siblings stay (default behavior)', async () => {
    const ts = 1782717800000;
    const fn = `${ts}-test01.md`;
    const sidecar = `${ts}-test01.options.json`;
    writeFileSync(
      join(stRoot, 'alice', 'inbox', fn),
      '---\nfrom: bob\n---\nhi\n'
    );
    writeFileSync(
      join(stRoot, 'alice', 'inbox', sidecar),
      '{"k":1}'
    );
    await coord.archive(
      asIdentity('alice'),
      fn as unknown as ReturnType<typeof asFilename>
    );
    expect(existsSync(join(stRoot, 'alice', 'archive', fn))).toBe(true);
    // Sibling unmoved.
    expect(existsSync(join(stRoot, 'alice', 'inbox', sidecar))).toBe(true);
    expect(existsSync(join(stRoot, 'alice', 'archive', sidecar))).toBe(false);
  });

  it('with opts.withAttachments=true, siblings move alongside', async () => {
    const ts = 1782717810000;
    const fn = `${ts}-test02.md`;
    const sidecar = `${ts}-test02.options.json`;
    writeFileSync(
      join(stRoot, 'alice', 'inbox', fn),
      '---\nfrom: bob\n---\nhi\n'
    );
    writeFileSync(
      join(stRoot, 'alice', 'inbox', sidecar),
      '{"k":1}'
    );
    await coord.archive(
      asIdentity('alice'),
      fn as unknown as ReturnType<typeof asFilename>,
      { withAttachments: true }
    );
    expect(existsSync(join(stRoot, 'alice', 'archive', fn))).toBe(true);
    expect(existsSync(join(stRoot, 'alice', 'archive', sidecar))).toBe(true);
    expect(existsSync(join(stRoot, 'alice', 'inbox', sidecar))).toBe(false);
  });
});

// ─── coord.archiveTrim opts.withAttachments ────────────────────────────

describe('coord.archiveTrim opts.withAttachments', () => {
  it('default leaves prefix-siblings in archive when their .md is trimmed', async () => {
    const ts = 1700000000000;
    const fn = `${ts}-old001.md`;
    const sidecar = `${ts}-old001.options.json`;
    writeFileSync(
      join(stRoot, 'alice', 'archive', fn),
      '---\nfrom: bob\n---\nhi\n'
    );
    writeFileSync(
      join(stRoot, 'alice', 'archive', sidecar),
      '{"k":1}'
    );
    const victims = await coord.archiveTrim(asIdentity('alice'), {
      olderThan: '1d',
      now: () => Date.now(),
    });
    expect(victims).toContain(fn);
    expect(existsSync(join(stRoot, 'alice', 'archive', fn))).toBe(false);
    expect(existsSync(join(stRoot, 'alice', 'archive', sidecar))).toBe(true);
  });

  it('withAttachments=true also deletes prefix-siblings', async () => {
    const ts = 1700000010000;
    const fn = `${ts}-old002.md`;
    const sidecar = `${ts}-old002.options.json`;
    writeFileSync(
      join(stRoot, 'alice', 'archive', fn),
      '---\nfrom: bob\n---\nhi\n'
    );
    writeFileSync(
      join(stRoot, 'alice', 'archive', sidecar),
      '{"k":1}'
    );
    await coord.archiveTrim(asIdentity('alice'), {
      olderThan: '1d',
      withAttachments: true,
      now: () => Date.now(),
    });
    expect(existsSync(join(stRoot, 'alice', 'archive', fn))).toBe(false);
    expect(existsSync(join(stRoot, 'alice', 'archive', sidecar))).toBe(false);
  });
});

// ─── coord.lsOrphans (brief-009 item 4 SDK parity) ─────────────────────

describe('coord.lsOrphans', () => {
  it('returns [] when no orphans exist', async () => {
    const r = await coord.lsOrphans();
    expect(r).toEqual([]);
  });

  it('surfaces sibling files whose .md is missing in the same folder', async () => {
    const ts = 1782717820000;
    const orphan = `${ts}-orph01.options.json`;
    writeFileSync(join(stRoot, 'alice', 'inbox', orphan), '{"k":1}');
    const r = await coord.lsOrphans();
    expect(r.map((it) => it.filename)).toContain(orphan);
    const item = r.find((it) => it.filename === orphan)!;
    expect(item.ts).toBe(ts);
  });

  it('skips siblings whose .md is present (not orphaned)', async () => {
    const ts = 1782717830000;
    const fn = `${ts}-paired.md`;
    const sidecar = `${ts}-paired.options.json`;
    writeFileSync(
      join(stRoot, 'alice', 'inbox', fn),
      '---\nfrom: bob\n---\nhi\n'
    );
    writeFileSync(
      join(stRoot, 'alice', 'inbox', sidecar),
      '{"k":1}'
    );
    const r = await coord.lsOrphans();
    expect(r.map((it) => it.filename)).not.toContain(sidecar);
  });

  it('opts.archive=true scans the archive folder', async () => {
    const ts = 1782717840000;
    const orphan = `${ts}-orph02.options.json`;
    writeFileSync(join(stRoot, 'alice', 'archive', orphan), '{"k":1}');
    const r = await coord.lsOrphans(undefined, { archive: true });
    expect(r.map((it) => it.filename)).toContain(orphan);
  });

  it('explicit identity scans a peer\'s folder', async () => {
    const ts = 1782717850000;
    const orphan = `${ts}-orph03.options.json`;
    writeFileSync(join(stRoot, 'bob', 'inbox', orphan), '{"k":1}');
    const r = await coord.lsOrphans(asIdentity('bob'));
    expect(r.map((it) => it.filename)).toContain(orphan);
  });
});

// ─── coord.ding (brief-009 item 4 SDK parity) ──────────────────────────

describe('coord.ding (handle wrapper around runDing)', () => {
  it('is a function on the handle', () => {
    expect(typeof coord.ding).toBe('function');
  });

  it('exits cleanly when the supplied signal aborts', async () => {
    const ac = new AbortController();
    const ptySend = async (): Promise<{ status: number; stderr: string }> => ({
      status: 0,
      stderr: '',
    });
    // Use a session-alive probe that always returns true so the
    // session-watch tick doesn't end the daemon on us.
    const isSessionAlive = (): boolean => true;
    const done = coord.ding({
      ptySession: 'fake-session',
      ptySend,
      isSessionAlive,
      sessionWatchIntervalMs: 10_000,
      tidyIntervalMs: 0,
      statusRefreshIntervalMs: 0,
      signal: ac.signal,
    });
    setTimeout(() => ac.abort(), 30);
    await done;
    // Reaching here without throwing is the assertion — the wrapper
    // successfully forwarded to runDing and runDing terminated.
    expect(true).toBe(true);
  });
});
