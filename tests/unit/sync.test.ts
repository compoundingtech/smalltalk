// tests/unit/sync.test.ts — coverage for cmd_sync (rsync mocked).

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

import {
  cmdSyncAll,
  cmdSyncAllPull,
  cmdSyncAllPush,
  cmdSyncPull,
  cmdSyncPush,
  cmdSyncSweep,
  parsePeersYaml,
  resolvePeer,
  type SyncContext,
  type SyncDeps,
} from '../../src/commands/sync.ts';

let scratch: string;
let stRoot: string;
let stConfig: string;

beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), 'coord-sync-test-'));
  stRoot = join(scratch, 'coord');
  stConfig = join(scratch, 'config');
  mkdirSync(stRoot, { recursive: true });
  mkdirSync(stConfig, { recursive: true });
});
afterEach(() => {
  rmSync(scratch, { recursive: true, force: true });
});

interface MockState {
  calls: string[][];
  exitCode: number;
  stderr?: string;
}

function makeMockRsync(state: MockState): SyncDeps {
  return {
    runRsync: (args) => {
      state.calls.push(args);
      const r: { status: number; stderr?: string } = { status: state.exitCode };
      if (state.stderr !== undefined) r.stderr = state.stderr;
      return r;
    },
    bannerSink: () => {},
  };
}

function ctx(deps: SyncDeps = {}): SyncContext {
  return { stRoot, stConfig, deps };
}

function setupIdentity(id: string): void {
  mkdirSync(join(stRoot, id, 'inbox'), { recursive: true });
  mkdirSync(join(stRoot, id, 'archive'), { recursive: true });
}

// ─── parsePeersYaml ─────────────────────────────────────────────────────

describe('parsePeersYaml', () => {
  it('parses simple key: value lines', () => {
    expect(
      parsePeersYaml('bobby: bob.example.com\nlaptop: laptop.example.com:/srv/coord\n')
    ).toEqual([
      { name: 'bobby', spec: 'bob.example.com' },
      { name: 'laptop', spec: 'laptop.example.com:/srv/coord' },
    ]);
  });

  it('skips comment lines', () => {
    expect(
      parsePeersYaml('# header\nbobby: bob.example.com\n# trailing\n')
    ).toEqual([{ name: 'bobby', spec: 'bob.example.com' }]);
  });

  it('skips blank lines', () => {
    expect(parsePeersYaml('\n\nbobby: bob.example.com\n\n')).toEqual([
      { name: 'bobby', spec: 'bob.example.com' },
    ]);
  });

  it('trims surrounding whitespace from name and value', () => {
    expect(parsePeersYaml('  bobby :   bob.example.com  \n')).toEqual([
      { name: 'bobby', spec: 'bob.example.com' },
    ]);
  });

  it('preserves embedded colons in the value (split on first only)', () => {
    expect(parsePeersYaml('laptop: host:/path/with/colon\n')).toEqual([
      { name: 'laptop', spec: 'host:/path/with/colon' },
    ]);
  });

  it('drops lines with empty name or empty value', () => {
    expect(parsePeersYaml(': nothing\nname:\n')).toEqual([]);
  });

  it('drops lines without a colon', () => {
    expect(parsePeersYaml('not yaml\n')).toEqual([]);
  });

  it('totally empty input → []', () => {
    expect(parsePeersYaml('')).toEqual([]);
  });
});

// ─── resolvePeer ────────────────────────────────────────────────────────

describe('resolvePeer', () => {
  it('local:<path> creates dir and returns "<path>/"', () => {
    const target = join(scratch, 'peer-local');
    expect(existsSync(target)).toBe(false);
    const r = resolvePeer(`local:${target}`, ctx());
    expect(r).toBe(`${target}/`);
    expect(existsSync(target)).toBe(true);
  });

  it('local: requires a path', () => {
    expect(() => resolvePeer('local:', ctx())).toThrowError(
      /local: peer requires a path/
    );
  });

  it('local:<path> strips trailing slash before re-adding', () => {
    const target = join(scratch, 'peer-local2');
    const r = resolvePeer(`local:${target}/`, ctx());
    expect(r).toBe(`${target}/`);
  });

  it('host:path → "host:path/"', () => {
    expect(resolvePeer('bob.example.com:/srv/coord', ctx())).toBe(
      'bob.example.com:/srv/coord/'
    );
  });

  it('bare hostname with no peers.yaml → "<host>:.local/state/coord/"', () => {
    expect(resolvePeer('bob.example.com', ctx())).toBe(
      'bob.example.com:.local/state/coord/'
    );
  });

  it('bare token resolves via peers.yaml alias', () => {
    writeFileSync(
      join(stConfig, 'peers.yaml'),
      'bobby: bob.example.com:/srv/coord\n'
    );
    expect(resolvePeer('bobby', ctx())).toBe(
      'bob.example.com:/srv/coord/'
    );
  });

  it('bare token unmapped in peers.yaml falls back to bare hostname', () => {
    writeFileSync(
      join(stConfig, 'peers.yaml'),
      'other: somewhere.example.com\n'
    );
    expect(resolvePeer('bobby', ctx())).toBe(
      'bobby:.local/state/coord/'
    );
  });

  it('alias targeting local: resolves through to local-style result', () => {
    const target = join(scratch, 'aliased');
    writeFileSync(
      join(stConfig, 'peers.yaml'),
      `peer: local:${target}\n`
    );
    expect(resolvePeer('peer', ctx())).toBe(`${target}/`);
  });
});

// ─── cmdSyncSweep ───────────────────────────────────────────────────────

describe('cmdSyncSweep', () => {
  it('empty root → removed=0, summary empty', () => {
    expect(cmdSyncSweep(ctx())).toEqual({ removed: 0, summary: '' });
  });

  it('byte-identical inbox/archive twin → removes inbox, summary singular', () => {
    setupIdentity('bob');
    writeFileSync(
      join(stRoot, 'bob', 'inbox', '1714826789010-aaaaaa.md'),
      'same'
    );
    writeFileSync(
      join(stRoot, 'bob', 'archive', '1714826789010-aaaaaa.md'),
      'same'
    );
    const r = cmdSyncSweep(ctx());
    expect(r.removed).toBe(1);
    expect(r.summary).toBe('# sweep: removed 1 redundant inbox file');
  });

  it('plural form for 2+', () => {
    setupIdentity('bob');
    for (const f of [
      '1714826789010-aaaaaa.md',
      '1714826789020-bbbbbb.md',
    ]) {
      writeFileSync(join(stRoot, 'bob', 'inbox', f), 'x');
      writeFileSync(join(stRoot, 'bob', 'archive', f), 'x');
    }
    const r = cmdSyncSweep(ctx());
    expect(r.removed).toBe(2);
    expect(r.summary).toBe('# sweep: removed 2 redundant inbox files');
  });
});

// ─── cmdSyncPush ────────────────────────────────────────────────────────

describe('cmdSyncPush', () => {
  it('invokes rsync with $ST_ROOT/ → resolved peer/', () => {
    setupIdentity('alice');
    const target = join(scratch, 'peer-target');
    const state: MockState = { calls: [], exitCode: 0 };
    cmdSyncPush(`local:${target}`, ctx(makeMockRsync(state)));
    expect(state.calls).toEqual([[`${stRoot}/`, `${target}/`]]);
  });

  it('rsync non-zero exit → throws "rsync push failed: ..."', () => {
    setupIdentity('alice');
    const state: MockState = {
      calls: [],
      exitCode: 23,
      stderr: 'rsync: simulated failure',
    };
    expect(() =>
      cmdSyncPush(`local:${join(scratch, 'peer')}`, ctx(makeMockRsync(state)))
    ).toThrowError(/rsync push failed/);
  });

  it('errors loudly when ST_ROOT does not exist', () => {
    rmSync(stRoot, { recursive: true });
    expect(() =>
      cmdSyncPush('local:/tmp/x', ctx(makeMockRsync({ calls: [], exitCode: 0 })))
    ).toThrowError(/no ST_ROOT to push from/);
  });

  it('runs sweep BEFORE rsync (zombies don\'t propagate)', () => {
    setupIdentity('alice');
    // Pre-existing zombie pair locally.
    const f = '1714826789010-aaaaaa.md';
    writeFileSync(join(stRoot, 'alice', 'inbox', f), 'same');
    writeFileSync(join(stRoot, 'alice', 'archive', f), 'same');
    const state: MockState = { calls: [], exitCode: 0 };
    cmdSyncPush(
      `local:${join(scratch, 'peer')}`,
      ctx(makeMockRsync(state))
    );
    // By the time rsync ran, the inbox copy was already gone (sweep before).
    expect(existsSync(join(stRoot, 'alice', 'inbox', f))).toBe(false);
    expect(state.calls).toHaveLength(1);
  });
});

// ─── cmdSyncPull ────────────────────────────────────────────────────────

describe('cmdSyncPull', () => {
  it('invokes rsync with peer/ → $ST_ROOT/', () => {
    setupIdentity('alice');
    const target = join(scratch, 'peer-source');
    mkdirSync(target);
    const state: MockState = { calls: [], exitCode: 0 };
    cmdSyncPull(`local:${target}`, ctx(makeMockRsync(state)));
    expect(state.calls).toEqual([[`${target}/`, `${stRoot}/`]]);
  });

  it('creates ST_ROOT if missing (mkdir -p)', () => {
    rmSync(stRoot, { recursive: true });
    expect(existsSync(stRoot)).toBe(false);
    cmdSyncPull(
      `local:${join(scratch, 'peer')}`,
      ctx(makeMockRsync({ calls: [], exitCode: 0 }))
    );
    expect(existsSync(stRoot)).toBe(true);
  });

  it('rsync non-zero exit → throws "rsync pull failed: ..."', () => {
    setupIdentity('alice');
    const state: MockState = { calls: [], exitCode: 1 };
    expect(() =>
      cmdSyncPull(`local:${join(scratch, 'peer')}`, ctx(makeMockRsync(state)))
    ).toThrowError(/rsync pull failed/);
  });
});

// ─── --all fan-outs ─────────────────────────────────────────────────────

describe('cmdSyncAllPush / cmdSyncAllPull', () => {
  it('errors when peers.yaml missing', () => {
    expect(() => cmdSyncAllPush(ctx(makeMockRsync({ calls: [], exitCode: 0 }))))
      .toThrowError(/no peers configured/);
    expect(() => cmdSyncAllPull(ctx(makeMockRsync({ calls: [], exitCode: 0 }))))
      .toThrowError(/no peers configured/);
  });

  it('errors when peers.yaml is empty / has no peers', () => {
    writeFileSync(join(stConfig, 'peers.yaml'), '# only comments\n');
    expect(() => cmdSyncAllPush(ctx(makeMockRsync({ calls: [], exitCode: 0 }))))
      .toThrowError(/no peers found/);
  });

  it('push --all calls rsync once per peer (PUSH direction only)', () => {
    setupIdentity('alice');
    writeFileSync(
      join(stConfig, 'peers.yaml'),
      [
        `bobby: local:${join(scratch, 'peer-bob')}`,
        `laptop: local:${join(scratch, 'peer-laptop')}`,
        '',
      ].join('\n')
    );
    const state: MockState = { calls: [], exitCode: 0 };
    cmdSyncAllPush(ctx(makeMockRsync(state)));
    expect(state.calls).toHaveLength(2);
    // All push direction: source = ST_ROOT, target = peer.
    for (const args of state.calls) {
      expect(args[0]).toBe(`${stRoot}/`);
      expect(args[1]!.startsWith(`${scratch}/peer-`)).toBe(true);
    }
  });

  it('pull --all is pull-only (one rsync per peer, peer→local direction)', () => {
    setupIdentity('alice');
    const peerTarget = join(scratch, 'peer-bob');
    mkdirSync(peerTarget);
    writeFileSync(
      join(stConfig, 'peers.yaml'),
      `bobby: local:${peerTarget}\n`
    );
    const state: MockState = { calls: [], exitCode: 0 };
    cmdSyncAllPull(ctx(makeMockRsync(state)));
    expect(state.calls).toEqual([[`${peerTarget}/`, `${stRoot}/`]]);
  });

  it('continues iterating peers even when one fails', () => {
    setupIdentity('alice');
    const peerA = join(scratch, 'peer-a');
    const peerB = join(scratch, 'peer-b');
    writeFileSync(
      join(stConfig, 'peers.yaml'),
      [`a: local:${peerA}`, `b: local:${peerB}`].join('\n')
    );
    let n = 0;
    const deps: SyncDeps = {
      runRsync: () => {
        n++;
        // First call (peer A) fails; second succeeds.
        return n === 1 ? { status: 23 } : { status: 0 };
      },
      bannerSink: () => {},
    };
    const r = cmdSyncAllPush(ctx(deps));
    expect(r.failures).toHaveLength(1);
    expect(r.successes).toEqual([`local:${peerB}`]);
  });
});

describe('cmdSyncAll (push + pull every peer)', () => {
  it('does push then pull per peer (2 rsync calls × N peers)', () => {
    setupIdentity('alice');
    const peerTarget = join(scratch, 'peer');
    writeFileSync(
      join(stConfig, 'peers.yaml'),
      `peer: local:${peerTarget}\n`
    );
    const state: MockState = { calls: [], exitCode: 0 };
    const r = cmdSyncAll(ctx(makeMockRsync(state)));
    // Push direction first, then pull direction.
    expect(state.calls).toHaveLength(2);
    expect(state.calls[0]).toEqual([`${stRoot}/`, `${peerTarget}/`]);
    expect(state.calls[1]).toEqual([`${peerTarget}/`, `${stRoot}/`]);
    expect(r.successes).toEqual([`local:${peerTarget}`]);
  });
});
