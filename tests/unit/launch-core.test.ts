// tests/unit/launch-core.test.ts — `st __launch-core` JSON entrypoint.
//
// Hidden entrypoint convoy uses to reach the launch write logic
// without depending on the `st launch` user CLI surface. Contract
// asserted here so a future refactor of `cmdLaunchCli` can't
// silently break convoy's bridge.

import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import type { CliContext } from '../../src/cli-context.ts';
import { cmdLaunchCoreCli } from '../../src/commands/launch-core.ts';

let scratch: string;
let ctx: CliContext;
let stdoutBuf: string;
let stderrBuf: string;
let stdinPayload: string;

function makeCtx(): CliContext {
  return {
    env: {
      ST_AGENT: 'alice',
      HOME: process.env.HOME ?? '/tmp',
    } as NodeJS.ProcessEnv,
    stRoot: scratch,
    stConfig: undefined,
    stdout: (s) => {
      stdoutBuf += s;
    },
    stderr: (s) => {
      stderrBuf += s;
    },
    readStdin: async () => Buffer.from(stdinPayload),
    stdinIsTty: () => false,
  } as unknown as CliContext;
}

beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), 'launch-core-'));
  stdoutBuf = '';
  stderrBuf = '';
  stdinPayload = '';
  ctx = makeCtx();
});

afterEach(() => {
  rmSync(scratch, { recursive: true, force: true });
});

describe('cmdLaunchCoreCli — happy path (JSON in → JSON out)', () => {
  it('valid claude dry-run JSON in → LaunchResult JSON on stdout, exit 0', async () => {
    stdinPayload = JSON.stringify({
      harness: 'claude',
      identity: 'alice',
      mcp: true, // post-cutover default is ding; opt into MCP for channel-mode
      dryRun: true,
    });
    const rc = await cmdLaunchCoreCli([], ctx);
    expect(rc).toBe(0);
    // stdout is valid JSON conforming to LaunchResult shape.
    const parsed = JSON.parse(stdoutBuf);
    expect(parsed.identity).toBe('alice');
    expect(parsed.channel).toBe(true); // claude --mcp
    expect(parsed.ding).toBe(false);
    expect(parsed.fresh).toBe(false);
    // argv includes --resume by default (non-fresh).
    expect(parsed.argv.join(' ')).toContain('--resume');
  });

  it('claude + --ding + --fresh threads through and yields the right result', async () => {
    stdinPayload = JSON.stringify({
      harness: 'claude',
      identity: 'alice',
      ding: true,
      fresh: true,
      dryRun: true,
    });
    const rc = await cmdLaunchCoreCli([], ctx);
    expect(rc).toBe(0);
    const parsed = JSON.parse(stdoutBuf);
    expect(parsed.ding).toBe(true);
    expect(parsed.fresh).toBe(true);
    expect(parsed.channel).toBe(false);
    expect(parsed.argv.join(' ')).not.toContain('--resume');
    expect(parsed.argv.join(' ')).not.toContain('--dangerously-load-development-channels');
  });

  it('codex harness works too', async () => {
    stdinPayload = JSON.stringify({
      harness: 'codex',
      identity: 'alice',
      dryRun: true,
    });
    const rc = await cmdLaunchCoreCli([], ctx);
    expect(rc).toBe(0);
    const parsed = JSON.parse(stdoutBuf);
    expect(parsed.argv[0]).toBe('codex');
  });

  it('extra JSON fields are IGNORED (forward-compat when future consumers send more)', async () => {
    stdinPayload = JSON.stringify({
      harness: 'claude',
      identity: 'alice',
      dryRun: true,
      // Bogus fields older smalltalk doesn't know about.
      futureField: 'value',
      anotherUnknown: 42,
    });
    const rc = await cmdLaunchCoreCli([], ctx);
    expect(rc).toBe(0);
    expect(JSON.parse(stdoutBuf).identity).toBe('alice');
  });

  it('missing optional fields use their defaults (no need to send every field)', async () => {
    // Minimal input — just harness. Everything else defaults.
    stdinPayload = JSON.stringify({ harness: 'claude' });
    const rc = await cmdLaunchCoreCli([], ctx);
    expect(rc).toBe(0);
    const parsed = JSON.parse(stdoutBuf);
    // Identity auto-generated as anon-<rand6> per env ST_AGENT='alice'
    // in fixture — actually alice.
    expect(parsed.identity).toBe('alice');
  });
});

describe('cmdLaunchCoreCli — validation errors (exit 1)', () => {
  it('stdin is not JSON → exit 1 + stderr says "not valid JSON"', async () => {
    stdinPayload = 'this is not json';
    const rc = await cmdLaunchCoreCli([], ctx);
    expect(rc).toBe(1);
    expect(stderrBuf).toContain('not valid JSON');
    expect(stdoutBuf).toBe('');
  });

  it('stdin is JSON but not an object → exit 1', async () => {
    stdinPayload = JSON.stringify(['not', 'an', 'object']);
    const rc = await cmdLaunchCoreCli([], ctx);
    expect(rc).toBe(1);
    expect(stderrBuf).toContain('input must be a JSON object');
  });

  it('harness missing → exit 1 + names the field', async () => {
    stdinPayload = JSON.stringify({ identity: 'alice', dryRun: true });
    const rc = await cmdLaunchCoreCli([], ctx);
    expect(rc).toBe(1);
    expect(stderrBuf).toContain('harness must be one of');
  });

  it('harness has wrong value → exit 1 + names the bad value', async () => {
    stdinPayload = JSON.stringify({ harness: 'gemini', dryRun: true });
    const rc = await cmdLaunchCoreCli([], ctx);
    expect(rc).toBe(1);
    expect(stderrBuf).toContain('harness must be one of claude, codex');
    expect(stderrBuf).toContain('"gemini"');
  });

  it('identity has wrong type → exit 1 + names the field', async () => {
    stdinPayload = JSON.stringify({ harness: 'claude', identity: 123 });
    const rc = await cmdLaunchCoreCli([], ctx);
    expect(rc).toBe(1);
    expect(stderrBuf).toContain('identity must be a string');
  });

  it('ding has wrong type (number instead of boolean) → exit 1', async () => {
    stdinPayload = JSON.stringify({
      harness: 'claude',
      ding: 1, // not a boolean
    });
    const rc = await cmdLaunchCoreCli([], ctx);
    expect(rc).toBe(1);
    expect(stderrBuf).toContain('ding must be a boolean');
  });

  it('extra positional argv → exit 1 + prints help', async () => {
    // The entrypoint takes no positional args; extra ones are
    // user error (usually a wrong invocation).
    stdinPayload = JSON.stringify({ harness: 'claude', dryRun: true });
    const rc = await cmdLaunchCoreCli(['some-arg'], ctx);
    expect(rc).toBe(1);
    expect(stderrBuf).toContain('unknown argument');
  });
});

describe('cmdLaunchCoreCli — --help', () => {
  it('--help returns 0 and describes the contract', async () => {
    const rc = await cmdLaunchCoreCli(['--help'], ctx);
    expect(rc).toBe(0);
    expect(stderrBuf).toContain('JSON-in/JSON-out entrypoint');
    expect(stderrBuf).toContain('LaunchInput');
    expect(stderrBuf).toContain('LaunchResult');
    expect(stderrBuf).toContain('convoy bridge');
    // stdout stays clean on --help.
    expect(stdoutBuf).toBe('');
  });
});

describe('cmdLaunchCoreCli — hidden from user surfaces', () => {
  it('the command is NOT in `st help` top-level listing', async () => {
    // Regression guard: convoy's bridge relies on this being
    // hidden. Adding it to help would encourage users to shell
    // out to it, which is exactly what we want to prevent
    // ("use convoy add, not __launch-core").
    const { runCli } = await import('../../src/cli.ts');
    await runCli(['--help'], ctx);
    // stderr (help output goes there) shouldn't mention the
    // hidden entrypoint by name.
    expect(stderrBuf).not.toContain('__launch-core');
    // stdout also clean (help doesn't emit it either).
    expect(stdoutBuf).not.toContain('__launch-core');
  });
});
