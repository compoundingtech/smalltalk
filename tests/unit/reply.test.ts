// tests/unit/reply.test.ts — `st message reply <thread>` CLI verb.
//
// The verb was missing through the ding-mode work: DING-BUS.md
// instructs every ding-mode agent to run `st message reply
// <filename> -m "<body>"` on inbox arrivals, but `dispatchMessage`
// in `src/cli.ts` didn't route `reply`. Every ding-mode agent
// errored with `unknown subcommand: reply` on their first response.
// This test file locks in the fix.

import {
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import type { CliContext } from '../../src/cli-context.ts';
import { cmdReply, cmdReplyCli } from '../../src/commands/reply.ts';

let scratch: string;
let coordRoot: string;
let ctx: CliContext;
let stdoutBuf: string;
let stderrBuf: string;

function plantMessage(
  identity: string,
  sub: 'inbox' | 'archive',
  filename: string,
  fm: Record<string, string>,
  body: string
): void {
  const dir = join(coordRoot, identity, sub);
  mkdirSync(dir, { recursive: true });
  const head = Object.entries(fm)
    .map(([k, v]) => `${k}: ${v}`)
    .join('\n');
  writeFileSync(join(dir, filename), `---\n${head}\n---\n${body}\n`);
}

beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), 'st-reply-'));
  coordRoot = scratch;
  stdoutBuf = '';
  stderrBuf = '';
  ctx = {
    env: {
      ST_AGENT: 'bob',
      ST_ROOT: coordRoot,
    } as NodeJS.ProcessEnv,
    coordRoot,
    coordConfig: undefined,
    stdout: (s) => {
      stdoutBuf += s;
    },
    stderr: (s) => {
      stderrBuf += s;
    },
    readStdin: async () => Buffer.alloc(0),
    stdinIsTty: () => true,
  } as unknown as CliContext;
});

afterEach(() => {
  rmSync(scratch, { recursive: true, force: true });
});

describe('cmdReply — programmatic reply to a message', () => {
  it('derives recipient from thread `from:` + writes reply into recipient inbox with inReplyTo set', async () => {
    // alice → bob. bob replies. Reply should land in alice's inbox
    // with inReplyTo pointing at the parent message.
    plantMessage(
      'bob',
      'inbox',
      '1783400000000-abcdef.md',
      { from: 'alice', subject: 'hello' },
      'hi bob'
    );
    const r = cmdReply({
      thread: '1783400000000-abcdef.md',
      body: 'hi alice',
      env: ctx.env,
      coordRoot,
    });
    // Reply lives in alice/inbox (recipient derived from parent's from:).
    expect(r.identity).toBe('alice');
    const aliceInbox = readdirSync(join(coordRoot, 'alice', 'inbox'));
    expect(aliceInbox).toContain(r.filename);
    // The reply body includes the frontmatter with inReplyTo + subject.
    // Note: no explicit `to:` in the frontmatter — the directory it
    // was written into IS the recipient (LAYOUT-004).
    const replyText = readFileSync(
      join(coordRoot, 'alice', 'inbox', r.filename),
      'utf8'
    );
    expect(replyText).toContain('from: bob');
    expect(replyText).toContain('in-reply-to: 1783400000000-abcdef.md');
    // Default subject: `re: <original>` when parent had a subject
    // (frontmatter quotes strings with colons; substring-check
    // covers both forms).
    expect(replyText).toContain('re: hello');
    expect(replyText).toContain('hi alice');
  });

  it('locates thread in bob\'s archive (not just inbox) so a post-archive reply still works', async () => {
    plantMessage(
      'bob',
      'archive',
      '1783400000001-abcdef.md',
      { from: 'alice', subject: 'archived' },
      'body'
    );
    const r = cmdReply({
      thread: '1783400000001-abcdef.md',
      body: 'reply to archived',
      env: ctx.env,
      coordRoot,
    });
    expect(r.identity).toBe('alice');
    // Subject still derives from the archived parent.
    const t = readFileSync(
      join(coordRoot, 'alice', 'inbox', r.filename),
      'utf8'
    );
    expect(t).toContain('re: archived');
  });

  it('locates thread in a peer\'s archive (cross-identity — post-sync case)', async () => {
    // alice ↔ carol conversation. bob's tree has carol's archive
    // (via sync); bob replies into carol's chain even though the
    // parent was never in bob's own inbox/archive.
    plantMessage(
      'carol',
      'archive',
      '1783400000002-abcdef.md',
      { from: 'alice', subject: 'peer-mail' },
      'x'
    );
    const r = cmdReply({
      thread: '1783400000002-abcdef.md',
      body: 'from bob',
      env: ctx.env,
      coordRoot,
    });
    expect(r.identity).toBe('alice');
  });

  it('subject override: --subject "custom" wins over the derived default', () => {
    plantMessage(
      'bob',
      'inbox',
      '1783400000003-abcdef.md',
      { from: 'alice', subject: 'hello' },
      'x'
    );
    const r = cmdReply({
      thread: '1783400000003-abcdef.md',
      body: 'hi',
      subject: 'custom-subject',
      env: ctx.env,
      coordRoot,
    });
    const t = readFileSync(
      join(coordRoot, 'alice', 'inbox', r.filename),
      'utf8'
    );
    expect(t).toContain('subject: custom-subject');
    expect(t).not.toContain('subject: re:');
  });

  it('no default subject when parent had no subject field', () => {
    plantMessage(
      'bob',
      'inbox',
      '1783400000004-abcdef.md',
      { from: 'alice' },
      'x'
    );
    const r = cmdReply({
      thread: '1783400000004-abcdef.md',
      body: 'hi',
      env: ctx.env,
      coordRoot,
    });
    const t = readFileSync(
      join(coordRoot, 'alice', 'inbox', r.filename),
      'utf8'
    );
    expect(t).not.toContain('subject:');
  });

  it('thread not found → throws MessageNotFoundError-shaped error', () => {
    plantMessage(
      'bob',
      'inbox',
      '1783400000005-aaaaaa.md',
      { from: 'alice' },
      'x'
    );
    // Well-formed filename that simply isn't planted anywhere.
    expect(() =>
      cmdReply({
        thread: '1783400000005-bbbbbb.md',
        body: 'x',
        env: ctx.env,
        coordRoot,
      })
    ).toThrow(/not found/i);
  });

  it('missing $ST_AGENT (and no --from) → throws with a clear message', () => {
    plantMessage(
      'bob',
      'inbox',
      '1783400000006-abcdef.md',
      { from: 'alice' },
      'x'
    );
    // Drop ST_AGENT.
    const envNoId = { ST_ROOT: coordRoot } as NodeJS.ProcessEnv;
    expect(() =>
      cmdReply({
        thread: '1783400000006-abcdef.md',
        body: 'x',
        env: envNoId,
        coordRoot,
      })
    ).toThrow(/ST_AGENT/);
  });
});

describe('cmdReplyCli — parses <thread> + -m + --subject and delegates', () => {
  it('reply with -m body writes into recipient inbox + prints filename', async () => {
    plantMessage(
      'bob',
      'inbox',
      '1783400000010-abcdef.md',
      { from: 'alice', subject: 'q' },
      'orig'
    );
    const rc = await cmdReplyCli(
      ['1783400000010-abcdef.md', '-m', 'inline body'],
      ctx
    );
    expect(rc).toBe(0);
    // Filename printed to stdout.
    expect(stdoutBuf).toMatch(/^\d+-[a-z0-9]+\.md\n$/);
    // Reply landed in alice's inbox.
    const files = readdirSync(join(coordRoot, 'alice', 'inbox'));
    expect(files).toHaveLength(1);
    const t = readFileSync(
      join(coordRoot, 'alice', 'inbox', files[0]!),
      'utf8'
    );
    expect(t).toContain('inline body');
  });

  it('reply --message is the long-form alias for -m', async () => {
    plantMessage(
      'bob',
      'inbox',
      '1783400000011-abcdef.md',
      { from: 'alice' },
      'x'
    );
    const rc = await cmdReplyCli(
      [
        '1783400000011-abcdef.md',
        '--message',
        'long-form',
      ],
      ctx
    );
    expect(rc).toBe(0);
    const files = readdirSync(join(coordRoot, 'alice', 'inbox'));
    const t = readFileSync(
      join(coordRoot, 'alice', 'inbox', files[0]!),
      'utf8'
    );
    expect(t).toContain('long-form');
  });

  it('reply with --subject overrides the derived default', async () => {
    plantMessage(
      'bob',
      'inbox',
      '1783400000012-abcdef.md',
      { from: 'alice', subject: 'hello' },
      'x'
    );
    await cmdReplyCli(
      [
        '1783400000012-abcdef.md',
        '-m',
        'body',
        '--subject',
        'CUSTOM',
      ],
      ctx
    );
    const files = readdirSync(join(coordRoot, 'alice', 'inbox'));
    const t = readFileSync(
      join(coordRoot, 'alice', 'inbox', files[0]!),
      'utf8'
    );
    expect(t).toContain('subject: CUSTOM');
    expect(t).not.toContain('re: hello');
  });

  it('reply --help prints usage and returns 0', async () => {
    const rc = await cmdReplyCli(['--help'], ctx);
    expect(rc).toBe(0);
    expect(stderrBuf).toContain('message reply');
    expect(stderrBuf).toContain('-m <body>');
  });

  it('reply with no thread arg → throws', async () => {
    await expect(cmdReplyCli(['-m', 'x'], ctx)).rejects.toThrow(
      /<thread-filename> is required/
    );
  });

  it('reply with -m AND piped stdin → throws (no silent drop)', async () => {
    // Match send.ts's guard: both sources present is a bug, not a
    // "silently pick one" situation. Set stdinIsTty=false + non-empty
    // readStdin.
    plantMessage(
      'bob',
      'inbox',
      '1783400000013-abcdef.md',
      { from: 'alice' },
      'x'
    );
    const conflictCtx: CliContext = {
      ...ctx,
      readStdin: async () => Buffer.from('piped body'),
      stdinIsTty: () => false,
    } as CliContext;
    await expect(
      cmdReplyCli(
        ['1783400000013-abcdef.md', '-m', 'inline'],
        conflictCtx
      )
    ).rejects.toThrow(/-m OR stdin, not both/);
  });
});

describe('dispatchMessage — `st message reply` is routed', () => {
  it('reply is a recognized message subcommand (not "unknown subcommand")', async () => {
    // The specific regression: `st message reply <fn>` used to error
    // with `unknown subcommand: reply` because the switch fell
    // through to the default. Regression guard.
    const { runCli } = await import('../../src/cli.ts');
    plantMessage(
      'bob',
      'inbox',
      '1783400000020-abcdef.md',
      { from: 'alice' },
      'x'
    );
    const rc = await runCli(
      ['message', 'reply', '1783400000020-abcdef.md', '-m', 'ok'],
      ctx
    );
    expect(rc).toBe(0);
    expect(stderrBuf).not.toContain('unknown subcommand');
  });
});
