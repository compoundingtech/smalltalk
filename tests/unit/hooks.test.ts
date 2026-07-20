// tests/unit/hooks.test.ts — `st hooks path`, the read-only hooks interface.

import {
  mkdirSync,
  mkdtempSync,
  readdirSync,
  rmSync,
  statSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { cmdHooksCli, resolveHooksInfo } from '../../src/commands/hooks.ts';
import type { CliContext } from '../../src/cli-context.ts';

let scratch: string;
let stdoutBuf: string;
let stderrBuf: string;
let ctx: CliContext;

beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), 'st-hooks-test-'));
  stdoutBuf = '';
  stderrBuf = '';
  ctx = {
    env: {},
    stRoot: '/unused',
    stConfig: '/unused',
    stdout: (s) => {
      stdoutBuf += s;
    },
    stderr: (s) => {
      stderrBuf += s;
    },
    readStdin: async () => Buffer.alloc(0),
  };
});
afterEach(() => {
  rmSync(scratch, { recursive: true, force: true });
});

/** A fake @compoundingtech/smalltalk install root with the claude-code hook scripts. */
function fakeRoot(opts: { missing?: string[]; stBin?: boolean } = {}): string {
  const root = join(scratch, 'install');
  const hooks = join(root, 'examples', 'claude-code', 'hooks');
  mkdirSync(hooks, { recursive: true });
  writeFileSync(join(root, 'package.json'), JSON.stringify({ name: '@compoundingtech/smalltalk' }));
  const all = [
    'session-start.sh',
    'pre-compact.sh',
    'pre-compact.impl.sh',
    'stop-failure.sh',
  ];
  for (const s of all) {
    if (opts.missing?.includes(s)) continue;
    writeFileSync(join(hooks, s), '#!/bin/sh\n');
  }
  if (opts.stBin !== false) {
    mkdirSync(join(root, 'bin'), { recursive: true });
    writeFileSync(join(root, 'bin', 'st'), '#!/bin/sh\n');
  }
  return root;
}

describe('resolveHooksInfo — claude-code', () => {
  it('all scripts present → scriptsPresent true, settings block + stBin resolved', () => {
    const root = fakeRoot();
    const info = resolveHooksInfo('claude-code', root)!;
    expect(info.family).toBe('claude-code');
    expect(info.hooksDir).toBe(join(root, 'examples', 'claude-code', 'hooks'));
    expect(info.stBin).toBe(join(root, 'bin', 'st'));
    expect(info.scriptsPresent).toBe(true);
    expect(info.scripts.map((s) => s.name)).toEqual([
      'session-start.sh',
      'pre-compact.sh',
      'pre-compact.impl.sh',
      'stop-failure.sh',
    ]);
    expect(info.scripts.every((s) => s.present)).toBe(true);
    // settings block wires the three hook events at absolute script paths
    const s = info.settings as {
      hooks: Record<string, { hooks: { command: string }[] }[]>;
    };
    expect(s.hooks.SessionStart[0]!.hooks[0]!.command).toBe(
      join(info.hooksDir, 'session-start.sh')
    );
    expect(s.hooks.PreCompact[0]!.hooks[0]!.command).toBe(
      join(info.hooksDir, 'pre-compact.sh')
    );
    expect(s.hooks.StopFailure[0]!.hooks[0]!.command).toBe(
      join(info.hooksDir, 'stop-failure.sh')
    );
  });

  it('a missing script → scriptsPresent false + that script present false (still returns info)', () => {
    const root = fakeRoot({ missing: ['pre-compact.impl.sh'] });
    const info = resolveHooksInfo('claude-code', root)!;
    expect(info.scriptsPresent).toBe(false);
    const impl = info.scripts.find((s) => s.name === 'pre-compact.impl.sh')!;
    expect(impl.present).toBe(false);
    // the others are still present
    expect(info.scripts.find((s) => s.name === 'session-start.sh')!.present).toBe(true);
  });

  it('no bin/st → stBin null', () => {
    const info = resolveHooksInfo('claude-code', fakeRoot({ stBin: false }))!;
    expect(info.stBin).toBeNull();
  });

  it('null root → null (cannot locate install)', () => {
    expect(resolveHooksInfo('claude-code', null)).toBeNull();
  });
});

describe('resolveHooksInfo — codex / pi', () => {
  it('codex → exampleConfig set, no settings block', () => {
    const root = join(scratch, 'install');
    mkdirSync(join(root, 'examples', 'codex'), { recursive: true });
    writeFileSync(join(root, 'package.json'), JSON.stringify({ name: '@compoundingtech/smalltalk' }));
    const info = resolveHooksInfo('codex', root)!;
    expect(info.settings).toBeUndefined();
    expect(info.exampleConfig).toBe(
      join(root, 'examples', 'codex', 'config.toml.example')
    );
  });

  it('pi → exampleConfig set, no settings block', () => {
    const root = join(scratch, 'install');
    mkdirSync(join(root, 'examples', 'pi'), { recursive: true });
    writeFileSync(join(root, 'package.json'), JSON.stringify({ name: '@compoundingtech/smalltalk' }));
    const info = resolveHooksInfo('pi', root)!;
    expect(info.settings).toBeUndefined();
    expect(info.exampleConfig).toBe(
      join(root, 'examples', 'pi', 'settings.example.json')
    );
  });
});

describe('resolveHooksInfo — read-only', () => {
  it('does not create or modify anything under the root', () => {
    const root = fakeRoot();
    const snap = (dir: string): string[] =>
      readdirSync(dir).flatMap((e) => {
        const p = join(dir, e);
        return statSync(p).isDirectory() ? snap(p) : [p];
      }).sort();
    const before = snap(root);
    resolveHooksInfo('claude-code', root);
    resolveHooksInfo('codex', root);
    expect(snap(root)).toEqual(before);
  });
});

describe('cmdHooksCli — against the real install', () => {
  it('path --json emits the documented top-level keys, exit 0', () => {
    const rc = cmdHooksCli(['path', '--json'], ctx);
    expect(rc).toBe(0);
    const out = JSON.parse(stdoutBuf) as Record<string, unknown>;
    for (const k of ['family', 'hooksDir', 'stBin', 'scriptsPresent', 'scripts', 'settings']) {
      expect(out).toHaveProperty(k);
    }
    expect(out.family).toBe('claude-code');
  });

  it('path (human) prints the hooks dir + a read-only note, exit 0', () => {
    const rc = cmdHooksCli(['path'], ctx);
    expect(rc).toBe(0);
    expect(stdoutBuf).toContain('Hook scripts:');
    expect(stdoutBuf).toContain('changed nothing');
  });

  it('--for codex → codex family', () => {
    const rc = cmdHooksCli(['path', '--for', 'codex', '--json'], ctx);
    expect(rc).toBe(0);
    expect((JSON.parse(stdoutBuf) as { family: string }).family).toBe('codex');
  });

  it('no subcommand → usage, exit 1', () => {
    expect(cmdHooksCli([], ctx)).toBe(1);
    expect(stderrBuf).toContain('usage:');
  });

  it('--help → usage, exit 0', () => {
    expect(cmdHooksCli(['--help'], ctx)).toBe(0);
    expect(stderrBuf).toContain('hooks path');
  });

  it('unknown subcommand → exit 1', () => {
    expect(cmdHooksCli(['nope'], ctx)).toBe(1);
    expect(stderrBuf).toContain('unknown subcommand: nope');
  });

  it('--for bogus → throws', () => {
    expect(() => cmdHooksCli(['path', '--for', 'bogus'], ctx)).toThrowError(/--for/);
  });

  it('unknown flag → throws', () => {
    expect(() => cmdHooksCli(['path', '--nope'], ctx)).toThrowError(/unknown flag/);
  });
});
