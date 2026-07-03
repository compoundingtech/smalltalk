// tools/cutover/rewrite-pty-toml.test.ts — pure-function coverage for
// the pty.toml cutover rewriter.

import { describe, expect, it } from 'vitest';
import { rewritePtyToml } from '../../../../tools/cutover/rewrite-pty-toml.ts';

describe('rewritePtyToml — server:coord → server:st', () => {
  it('rewrites server:coord inside a command string', () => {
    const input = [
      'prefix = "cos"',
      '',
      '[sessions.claude]',
      'command = "$HOME/bin/pty-claude-launcher.sh --dangerously-load-development-channels server:coord"',
      '',
      '[sessions.claude.env]',
      'ST_AGENT = "cos"',
      '',
    ].join('\n');
    const r = rewritePtyToml(input);
    expect(r.changed).toBe(true);
    expect(r.text).toContain('server:st');
    expect(r.text).not.toContain('server:coord');
    expect(r.actions.some((a) => a.startsWith('command:'))).toBe(true);
  });

  it('leaves server:coord-web (word-boundary mismatch) alone', () => {
    // Contrived but proves the \b guard — a session name that
    // starts with "coord" isn't mistaken for the server:coord token.
    const input = [
      '[sessions.claude]',
      'command = "some-launcher --server server:coord-web"',
    ].join('\n');
    const r = rewritePtyToml(input);
    expect(r.changed).toBe(false);
  });
});

describe('rewritePtyToml — coord ding → st ding', () => {
  it('rewrites coord ding inside a command string', () => {
    const input = [
      '[sessions.codex]',
      'command = "codex --model bar"',
      'tags = { role = "agent" }',
      '',
      '[sessions.ding]',
      'command = "coord ding codex --identity alice"',
      'tags = { role = "ding", strategy = "permanent" }',
      '',
    ].join('\n');
    const r = rewritePtyToml(input);
    expect(r.changed).toBe(true);
    expect(r.text).toContain('command = "st ding codex --identity alice"');
    expect(r.text).not.toContain('coord ding');
  });

  it('does not mis-match `coord dingus` (word-boundary guard)', () => {
    const input = [
      '[sessions.ding]',
      'command = "coord dingus foo"',
    ].join('\n');
    const r = rewritePtyToml(input);
    // `\bcoord ding\b` requires a word boundary AFTER ding, and
    // dingus has a `u` right after — no match, no rewrite.
    expect(r.changed).toBe(false);
  });
});

describe('rewritePtyToml — env-var renames inside [sessions.X.env]', () => {
  it('renames COORD_IDENTITY → ST_AGENT when ST_AGENT not already set', () => {
    const input = [
      '[sessions.claude.env]',
      'COORD_IDENTITY = "cos"',
      'CLAUDE_PERMISSION_MODE = "bypassPermissions"',
      '',
    ].join('\n');
    const r = rewritePtyToml(input);
    expect(r.changed).toBe(true);
    expect(r.text).toContain('ST_AGENT = "cos"');
    expect(r.text).not.toContain('COORD_IDENTITY');
    // Adjacent non-cutover line untouched, in place.
    expect(r.text).toContain('CLAUDE_PERMISSION_MODE = "bypassPermissions"');
  });

  it('drops redundant COORD_IDENTITY when ST_AGENT is already set (belt-and-suspenders)', () => {
    const input = [
      '[sessions.claude.env]',
      'ST_AGENT = "evals-claude"',
      'ST_IDENTITY = "evals-claude"',
      'COORD_IDENTITY = "evals-claude"',
      '',
    ].join('\n');
    const r = rewritePtyToml(input);
    expect(r.changed).toBe(true);
    expect(r.text).toContain('ST_AGENT = "evals-claude"');
    expect(r.text).not.toContain('COORD_IDENTITY');
    expect(r.text).not.toContain('ST_IDENTITY');
    expect(
      r.actions.filter((a) => a.startsWith('env: dropped redundant')).length
    ).toBe(2);
  });

  it('renames COORD_ROOT → ST_ROOT and COORD_CONFIG → ST_CONFIG', () => {
    const input = [
      '[sessions.claude.env]',
      'COORD_ROOT = "/tmp/root"',
      'COORD_CONFIG = "/tmp/cfg"',
      '',
    ].join('\n');
    const r = rewritePtyToml(input);
    expect(r.changed).toBe(true);
    expect(r.text).toContain('ST_ROOT = "/tmp/root"');
    expect(r.text).toContain('ST_CONFIG = "/tmp/cfg"');
    expect(r.text).not.toContain('COORD_ROOT');
    expect(r.text).not.toContain('COORD_CONFIG');
  });

  it('does not rewrite env-shaped lines outside a `.env` block', () => {
    // A rogue `COORD_IDENTITY = "x"` outside `[sessions.<name>.env]`
    // is NOT an env var (probably a comment or a top-level key that
    // pty doesn't consume) — leave it alone.
    const input = [
      'COORD_IDENTITY = "alice"',
      '',
      '[sessions.claude]',
      'command = "claude"',
      '',
    ].join('\n');
    const r = rewritePtyToml(input);
    expect(r.changed).toBe(false);
  });

  it('a second legacy key in the same block after a rename gets the drop path', () => {
    const input = [
      '[sessions.claude.env]',
      'COORD_IDENTITY = "cos"',
      'ST_IDENTITY = "cos"',
      '',
    ].join('\n');
    const r = rewritePtyToml(input);
    // First line renames COORD_IDENTITY → ST_AGENT.
    // Second line (ST_IDENTITY) now sees ST_AGENT as canonical-set,
    // so drops.
    expect(r.changed).toBe(true);
    expect(r.text).toContain('ST_AGENT = "cos"');
    expect(r.text).not.toContain('ST_IDENTITY');
    expect(r.text).not.toContain('COORD_IDENTITY');
  });
});

describe('rewritePtyToml — full-file end-to-end shapes', () => {
  it('the canonical cos/pty.toml shape rewrites cleanly', () => {
    const input = [
      'prefix = "cos"',
      '',
      '[sessions.claude]',
      'command = "$HOME/bin/pty-claude-launcher.sh --dangerously-load-development-channels server:coord"',
      'tags = { role = "agent", strategy = "permanent" }',
      '',
      '[sessions.claude.env]',
      'COORD_IDENTITY = "cos"',
      '# cos spawns autonomous sub-agents, so it opts into un-gated bypass.',
      'CLAUDE_PERMISSION_MODE = "bypassPermissions"',
      '',
    ].join('\n');
    const r = rewritePtyToml(input);
    expect(r.changed).toBe(true);
    // Load-bearing rewrites landed:
    expect(r.text).toContain('server:st');
    expect(r.text).toContain('ST_AGENT = "cos"');
    // Comment preserved verbatim:
    expect(r.text).toContain(
      '# cos spawns autonomous sub-agents, so it opts into un-gated bypass.'
    );
    // CLAUDE_PERMISSION_MODE untouched:
    expect(r.text).toContain('CLAUDE_PERMISSION_MODE = "bypassPermissions"');
    // No cutover markers left behind:
    expect(r.text).not.toContain('coord');
    expect(r.text).not.toContain('COORD');
  });

  it('idempotent: rewriting a post-cutover file is a no-op', () => {
    const input = [
      'prefix = "cos"',
      '',
      '[sessions.claude]',
      'command = "$HOME/bin/pty-claude-launcher.sh --dangerously-load-development-channels server:st"',
      '',
      '[sessions.claude.env]',
      'ST_AGENT = "cos"',
      '',
    ].join('\n');
    const r = rewritePtyToml(input);
    expect(r.changed).toBe(false);
    expect(r.text).toBe(input);
  });

  it('multi-session file: each block is scoped independently', () => {
    const input = [
      '[sessions.claude.env]',
      'ST_AGENT = "cos"',
      'COORD_IDENTITY = "cos"',  // redundant here — should drop
      '',
      '[sessions.other.env]',
      'COORD_IDENTITY = "otheragent"',  // no ST_AGENT — should rename
      '',
    ].join('\n');
    const r = rewritePtyToml(input);
    expect(r.text).toContain('ST_AGENT = "cos"');
    expect(r.text).toContain('ST_AGENT = "otheragent"');
    expect(r.text).not.toContain('COORD_IDENTITY');
  });
});

describe('rewritePtyToml — no-op cases', () => {
  it('empty file returns unchanged', () => {
    const r = rewritePtyToml('');
    expect(r.changed).toBe(false);
    expect(r.text).toBe('');
  });

  it('file with only unrelated content returns unchanged', () => {
    const input = 'prefix = "myrepo"\n\n[sessions.x]\ncommand = "sh"\n';
    const r = rewritePtyToml(input);
    expect(r.changed).toBe(false);
    expect(r.text).toBe(input);
  });
});
