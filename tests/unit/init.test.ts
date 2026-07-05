// tests/unit/init.test.ts — `coord init` writes/merges .mcp.json.
//
// brief-026: surgical idempotent merge on mcpServers.st; preserves
// other servers; prompt-gated on divergent existing entries; atomic
// write; portable binary path resolution.

import {
  chmodSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import type { CliContext } from '../../src/cli-context.ts';
import {
  cmdInit,
  resolveCoordBinPath,
} from '../../src/commands/init.ts';

const FAKE_BIN = '/usr/local/bin/coord';

let scratch: string;

beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), 'coord-init-'));
});

afterEach(() => {
  // Restore write perms if a test munged them, then rm.
  try {
    chmodSync(scratch, 0o755);
  } catch {
    // ignore
  }
  rmSync(scratch, { recursive: true, force: true });
});

interface RecordedCtx extends CliContext {
  readonly stdoutBuf: string;
  readonly stderrBuf: string;
}

function makeCtx(stdin = ''): RecordedCtx {
  const stdoutBuf = { v: '' };
  const stderrBuf = { v: '' };
  const ctx: RecordedCtx = {
    env: {} as NodeJS.ProcessEnv,
    coordRoot: '/unused',
    coordConfig: '/unused',
    stdout: (s) => {
      stdoutBuf.v += s;
    },
    stderr: (s) => {
      stderrBuf.v += s;
    },
    readStdin: async () => Buffer.from(stdin, 'utf8'),
    get stdoutBuf() {
      return stdoutBuf.v;
    },
    get stderrBuf() {
      return stderrBuf.v;
    },
  } as RecordedCtx;
  return ctx;
}

function readJson(path: string): Record<string, unknown> {
  return JSON.parse(readFileSync(path, 'utf8')) as Record<string, unknown>;
}

// ─── Bin path resolution ────────────────────────────────────────────────

describe('resolveCoordBinPath', () => {
  it('returns a path that exists on disk', () => {
    const p = resolveCoordBinPath();
    expect(existsSync(p)).toBe(true);
    expect(statSync(p).isFile()).toBe(true);
  });

  it('does not return a hardcoded /Volumes/... path', () => {
    // Defensive: the only way this test would fail on a non-mac dev
    // machine is if someone hardcoded a developer path. The check is
    // narrow on purpose — `/Users/...` and `/home/...` are legitimate
    // install locations on real machines.
    const p = resolveCoordBinPath();
    // The path may *coincidentally* live under /Volumes when the repo
    // checkout itself does — that's fine, we just want to verify it's
    // derived from this module's location, not a literal.
    // (No hardcoded literal exists in the source; the regression
    // guard is the source-grep test below.)
    expect(p.endsWith('/bin/coord')).toBe(true);
  });
});

// ─── Empty / new file ───────────────────────────────────────────────────

describe('cmdInit — write new .mcp.json', () => {
  it('empty dir → writes coord entry as the only mcpServer', async () => {
    const ctx = makeCtx();
    const r = await cmdInit(
      { dir: scratch, binPath: FAKE_BIN },
      ctx
    );
    expect(r.outcome).toBe('wrote-new');
    const target = join(scratch, '.mcp.json');
    expect(existsSync(target)).toBe(true);
    const parsed = readJson(target);
    expect(parsed).toEqual({
      mcpServers: {
        st: {
          type: 'stdio',
          command: FAKE_BIN,
          args: ['mcp', '--channel'],
          env: {},
        },
      },
    });
  });

  it('--no-channel writes args without `--channel`', async () => {
    const ctx = makeCtx();
    const r = await cmdInit(
      { dir: scratch, binPath: FAKE_BIN, noChannel: true },
      ctx
    );
    expect(r.outcome).toBe('wrote-new');
    const parsed = readJson(join(scratch, '.mcp.json'));
    const st = (
      parsed.mcpServers as Record<string, { args: string[] }>
    ).st;
    expect(st.args).toEqual(['mcp']);
  });
});

// ─── Merge into existing ────────────────────────────────────────────────

describe('cmdInit — merge into existing .mcp.json', () => {
  it('preserves other mcpServers entries when adding st', async () => {
    writeFileSync(
      join(scratch, '.mcp.json'),
      JSON.stringify(
        {
          mcpServers: {
            'other-server': {
              type: 'stdio',
              command: '/usr/local/bin/other',
              args: ['serve'],
            },
          },
        },
        null,
        2
      )
    );
    const ctx = makeCtx();
    const r = await cmdInit(
      { dir: scratch, binPath: FAKE_BIN },
      ctx
    );
    expect(r.outcome).toBe('merged-into-existing');
    const parsed = readJson(join(scratch, '.mcp.json'));
    const servers = parsed.mcpServers as Record<string, unknown>;
    expect(Object.keys(servers).sort()).toEqual(['other-server', 'st']);
    expect((servers['other-server'] as { command: string }).command).toBe(
      '/usr/local/bin/other'
    );
  });

  it('preserves non-mcpServers top-level keys', async () => {
    writeFileSync(
      join(scratch, '.mcp.json'),
      JSON.stringify({ otherTopLevel: { a: 1 }, mcpServers: {} }, null, 2)
    );
    const ctx = makeCtx();
    await cmdInit({ dir: scratch, binPath: FAKE_BIN }, ctx);
    const parsed = readJson(join(scratch, '.mcp.json'));
    expect(parsed.otherTopLevel).toEqual({ a: 1 });
  });

  it('byte-identical st entry → already-configured no-op', async () => {
    writeFileSync(
      join(scratch, '.mcp.json'),
      JSON.stringify(
        {
          mcpServers: {
            st: {
              type: 'stdio',
              command: FAKE_BIN,
              args: ['mcp', '--channel'],
              env: {},
            },
          },
        },
        null,
        2
      )
    );
    const ctx = makeCtx();
    const r = await cmdInit(
      { dir: scratch, binPath: FAKE_BIN },
      ctx
    );
    expect(r.outcome).toBe('already-configured');
    expect(ctx.stderrBuf).toMatch(/already has matching st entry/);
  });

  it('byte-identical legacy `coord` entry → already-configured no-op (back-compat)', async () => {
    // A pre-cutover .mcp.json that still uses the `coord` key is
    // still recognized. init reads both keys, prefers `st` when both
    // are present. This test proves the coord back-compat read path.
    writeFileSync(
      join(scratch, '.mcp.json'),
      JSON.stringify(
        {
          mcpServers: {
            coord: {
              type: 'stdio',
              command: FAKE_BIN,
              args: ['mcp', '--channel'],
              env: {},
            },
          },
        },
        null,
        2
      )
    );
    const ctx = makeCtx();
    const r = await cmdInit(
      { dir: scratch, binPath: FAKE_BIN },
      ctx
    );
    expect(r.outcome).toBe('already-configured');
    expect(ctx.stderrBuf).toMatch(/already has matching coord entry/);
  });
});

// ─── Divergent existing + --force / prompt ──────────────────────────────

describe('cmdInit — divergent existing entry', () => {
  function plantDivergentEntry(): void {
    // Plants a divergent LEGACY `coord:` entry (worst-case: pre-
    // cutover file) to exercise both back-compat read + overwrite +
    // legacy-key drop on the write. Overwrite must land as `st:`
    // (post-cutover shape) and remove the `coord:` key.
    writeFileSync(
      join(scratch, '.mcp.json'),
      JSON.stringify(
        {
          mcpServers: {
            coord: {
              type: 'stdio',
              command: '/old/path/to/coord',
              args: ['mcp'],
              env: {},
            },
          },
        },
        null,
        2
      )
    );
  }

  it('--force → overwrites cleanly', async () => {
    plantDivergentEntry();
    const ctx = makeCtx();
    const r = await cmdInit(
      { dir: scratch, binPath: FAKE_BIN, force: true },
      ctx
    );
    expect(r.outcome).toBe('overwrote-divergent');
    const parsed = readJson(join(scratch, '.mcp.json'));
    const st = (parsed.mcpServers as Record<string, { command: string }>)
      .st;
    expect(st.command).toBe(FAKE_BIN);
  });

  it('no --force, answers "n" → skipped, original untouched', async () => {
    plantDivergentEntry();
    const before = readFileSync(join(scratch, '.mcp.json'), 'utf8');
    const ctx = makeCtx();
    const r = await cmdInit(
      { dir: scratch, binPath: FAKE_BIN, promptAnswer: 'n' },
      ctx
    );
    expect(r.outcome).toBe('skipped-by-user');
    expect(readFileSync(join(scratch, '.mcp.json'), 'utf8')).toBe(before);
    expect(ctx.stderrBuf).toMatch(/skipped/);
  });

  it('no --force, answers "y" → overwrites', async () => {
    plantDivergentEntry();
    const ctx = makeCtx();
    const r = await cmdInit(
      { dir: scratch, binPath: FAKE_BIN, promptAnswer: 'y' },
      ctx
    );
    expect(r.outcome).toBe('overwrote-divergent');
    const parsed = readJson(join(scratch, '.mcp.json'));
    const st = (parsed.mcpServers as Record<string, { command: string }>)
      .st;
    expect(st.command).toBe(FAKE_BIN);
  });
});

// ─── --print mode ───────────────────────────────────────────────────────

describe('cmdInit — --print', () => {
  it('emits JSON to stdout and touches no disk', async () => {
    const ctx = makeCtx();
    const r = await cmdInit(
      { dir: scratch, binPath: FAKE_BIN, print: true },
      ctx
    );
    expect(r.outcome).toBe('printed-only');
    expect(existsSync(join(scratch, '.mcp.json'))).toBe(false);
    // Parse stdout to confirm the entry shape.
    const parsed = JSON.parse(ctx.stdoutBuf) as Record<string, unknown>;
    expect(parsed.mcpServers).toBeDefined();
    expect(
      (parsed.mcpServers as Record<string, { command: string }>).st
        .command
    ).toBe(FAKE_BIN);
  });
});

// ─── Error paths ────────────────────────────────────────────────────────

describe('cmdInit — error handling', () => {
  it('malformed existing JSON → loud error, no partial write', async () => {
    const target = join(scratch, '.mcp.json');
    writeFileSync(target, '{ this is not json');
    const before = readFileSync(target, 'utf8');
    const ctx = makeCtx();
    await expect(
      cmdInit({ dir: scratch, binPath: FAKE_BIN }, ctx)
    ).rejects.toThrow(/not valid JSON/);
    // Original file unchanged.
    expect(readFileSync(target, 'utf8')).toBe(before);
  });

  it('target dir does not exist → loud error', async () => {
    const ctx = makeCtx();
    await expect(
      cmdInit(
        { dir: join(scratch, 'does-not-exist'), binPath: FAKE_BIN },
        ctx
      )
    ).rejects.toThrow(/does not exist/);
  });

  it('atomic write: read-only parent dir → throws, existing file unchanged', async () => {
    // Plant a known-good file, then make the dir read-only so the
    // tmp-sibling write fails. The original must survive intact.
    const target = join(scratch, '.mcp.json');
    writeFileSync(
      target,
      JSON.stringify(
        {
          mcpServers: {
            'other-server': {
              type: 'stdio',
              command: '/old/other',
              args: ['serve'],
            },
          },
        },
        null,
        2
      )
    );
    const before = readFileSync(target, 'utf8');
    chmodSync(scratch, 0o555); // read+exec only
    const ctx = makeCtx();
    try {
      await expect(
        cmdInit({ dir: scratch, binPath: FAKE_BIN }, ctx)
      ).rejects.toThrow();
    } finally {
      chmodSync(scratch, 0o755); // restore so afterEach can rm
    }
    expect(readFileSync(target, 'utf8')).toBe(before);
  });
});

// ─── Source-level regression: no hardcoded developer paths ──────────────

describe('source guard — no hardcoded developer paths in init.ts', () => {
  it('init.ts does not contain a literal `/Volumes/` or `/Users/myobie`', () => {
    // brief-026 boundary: the verb is meant to *eliminate* hardcoded
    // paths, so the implementation itself must not contain one.
    const src = readFileSync(
      new URL('../../src/commands/init.ts', import.meta.url),
      'utf8'
    );
    expect(src).not.toMatch(/\/Volumes\//);
    expect(src).not.toMatch(/\/Users\/myobie/);
  });
});
