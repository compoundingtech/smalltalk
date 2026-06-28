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

import { createCoord, type Coord } from '../../src/lib.ts';
import { InvalidIdentityError } from '../../src/errors.ts';
import { asIdentity, type Identity } from '../../src/types.ts';
import type {
  MemberSummary,
  MemberSummaryEnriched,
} from '../../src/commands/members.ts';
import type { Overview } from '../../src/commands/overview.ts';

let scratch: string;
let coordRoot: string;
let coord: Coord;

function setupIdentity(id: string): void {
  mkdirSync(join(coordRoot, id, 'inbox'), { recursive: true });
  mkdirSync(join(coordRoot, id, 'archive'), { recursive: true });
}

function setStatus(id: string, value: string): void {
  writeFileSync(join(coordRoot, id, 'status'), `${value}\n`);
}

beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), 'coord-lib-api-'));
  coordRoot = join(scratch, 'coord');
  mkdirSync(coordRoot, { recursive: true });
  setupIdentity('alice');
  setupIdentity('bob');
  setupIdentity('carol');
  coord = createCoord({
    root: coordRoot,
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
    rmSync(coordRoot, { recursive: true, force: true });
    mkdirSync(coordRoot, { recursive: true });
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
        join(coordRoot, 'alice', 'inbox', actual),
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
    expect(existsSync(join(coordRoot, 'dave', 'inbox'))).toBe(true);
    expect(existsSync(join(coordRoot, 'dave', 'archive'))).toBe(true);
  });

  it('existing name → { created: false } (idempotent)', async () => {
    // alice was set up in beforeEach with both folders.
    const r = await coord.createIdentity('alice');
    expect(r).toEqual({ created: false });
  });

  it('partial state (only inbox exists) → completes the layout, returns { created: true }', async () => {
    // A sender may have lazily mkdir'd `eve/inbox/` via coord_msg_send;
    // archive is missing. createIdentity should backfill.
    mkdirSync(join(coordRoot, 'eve', 'inbox'), { recursive: true });
    const r = await coord.createIdentity('eve');
    expect(r).toEqual({ created: true });
    expect(existsSync(join(coordRoot, 'eve', 'archive'))).toBe(true);
  });

  it('rejects an invalid identity grammar', async () => {
    await expect(coord.createIdentity('INVALID')).rejects.toThrow(
      InvalidIdentityError
    );
  });

  it('rejects a reserved name (e.g. `journal`)', async () => {
    // RESERVED_NAMES includes folder/sidecar names + state words +
    // verb names — validIdentity guards them. Sample one.
    await expect(coord.createIdentity('journal')).rejects.toThrow(
      InvalidIdentityError
    );
  });

  it('rejects the brief-022 derived state `unknown` as an identity', async () => {
    // Regression for the brief-024 carry-over reserving `unknown`.
    await expect(coord.createIdentity('unknown')).rejects.toThrow(
      InvalidIdentityError
    );
  });

  it('does NOT set status or create the journal folder', async () => {
    await coord.createIdentity('frank');
    expect(existsSync(join(coordRoot, 'frank', 'status'))).toBe(false);
    expect(existsSync(join(coordRoot, 'frank', 'journal'))).toBe(false);
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
      'journal',
      'status',
      'unknown',
      'available',
    ]) {
      expect(mod.RESERVED_NAMES).toContain(name);
    }
  });
});
