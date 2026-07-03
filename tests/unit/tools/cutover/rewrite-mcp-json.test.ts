// tools/cutover/rewrite-mcp-json.test.ts — pure-function coverage for
// the .mcp.json cutover rewriter.

import { describe, expect, it } from 'vitest';
import { rewriteMcpJson } from '../../../../tools/cutover/rewrite-mcp-json.ts';

describe('rewriteMcpJson — key rename mcpServers.coord → mcpServers.st', () => {
  it('renames the coord entry to st and preserves its shape', () => {
    const input = JSON.stringify(
      {
        mcpServers: {
          coord: {
            type: 'stdio',
            command: '/Volumes/SSD/src/github.com/myobie/smalltalk/bin/st',
            args: ['mcp', '--channel'],
            env: {},
          },
        },
      },
      null,
      2
    ) + '\n';
    const r = rewriteMcpJson(input);
    expect(r.changed).toBe(true);
    expect(r.actions).toContain('renamed mcpServers.coord → mcpServers.st');
    const parsed = JSON.parse(r.text);
    expect(parsed.mcpServers.st).toBeDefined();
    expect(parsed.mcpServers.coord).toBeUndefined();
    expect(parsed.mcpServers.st.args).toEqual(['mcp', '--channel']);
    // Trailing newline preserved.
    expect(r.text.endsWith('\n')).toBe(true);
  });

  it('when both coord and st entries exist, drops coord and keeps st untouched', () => {
    const input = JSON.stringify(
      {
        mcpServers: {
          coord: { command: 'x' },
          st: { command: 'y', args: ['mcp'] },
        },
      },
      null,
      2
    );
    const r = rewriteMcpJson(input);
    expect(r.changed).toBe(true);
    const parsed = JSON.parse(r.text);
    expect(parsed.mcpServers.coord).toBeUndefined();
    expect(parsed.mcpServers.st).toEqual({ command: 'y', args: ['mcp'] });
    expect(r.actions).toContain(
      'dropped mcpServers.coord (mcpServers.st already present)'
    );
  });

  it('idempotent: rewrite of already-cutover file is a no-op', () => {
    const input = JSON.stringify(
      {
        mcpServers: {
          st: {
            type: 'stdio',
            command: '/Volumes/SSD/src/github.com/myobie/smalltalk/bin/st',
            args: ['mcp', '--channel'],
            env: { ST_AGENT: 'alice', ST_ROOT: '/tmp/root' },
          },
        },
      },
      null,
      2
    );
    const r = rewriteMcpJson(input);
    expect(r.changed).toBe(false);
    expect(r.actions).toEqual([]);
    expect(r.text).toBe(input);
  });
});

describe('rewriteMcpJson — command path rewrites', () => {
  it('rewrites the old-repo path myobie/coord/bin/coord → smalltalk/bin/st', () => {
    const input = JSON.stringify({
      mcpServers: {
        coord: {
          command: '/Volumes/SSD/src/github.com/myobie/coord/bin/coord',
        },
      },
    });
    const r = rewriteMcpJson(input);
    expect(r.changed).toBe(true);
    const parsed = JSON.parse(r.text);
    expect(parsed.mcpServers.st.command).toBe(
      '/Volumes/SSD/src/github.com/myobie/smalltalk/bin/st'
    );
  });

  it('rewrites myobie/smalltalk/bin/coord → myobie/smalltalk/bin/st', () => {
    const input = JSON.stringify({
      mcpServers: {
        coord: {
          command: '/Volumes/SSD/src/github.com/myobie/smalltalk/bin/coord',
        },
      },
    });
    const r = rewriteMcpJson(input);
    expect(r.changed).toBe(true);
    const parsed = JSON.parse(r.text);
    expect(parsed.mcpServers.st.command).toBe(
      '/Volumes/SSD/src/github.com/myobie/smalltalk/bin/st'
    );
  });

  it('rewrites myobie/coord/bin/st → myobie/smalltalk/bin/st (still-legacy repo path)', () => {
    const input = JSON.stringify({
      mcpServers: {
        coord: {
          command: '/Volumes/SSD/src/github.com/myobie/coord/bin/st',
        },
      },
    });
    const r = rewriteMcpJson(input);
    expect(r.changed).toBe(true);
    const parsed = JSON.parse(r.text);
    expect(parsed.mcpServers.st.command).toBe(
      '/Volumes/SSD/src/github.com/myobie/smalltalk/bin/st'
    );
  });

  it('leaves the terminal shape myobie/smalltalk/bin/st alone', () => {
    // Key needs to also be `st` for full no-op — key rename runs
    // independently of command rewrite.
    const input = JSON.stringify({
      mcpServers: {
        st: {
          command: '/Volumes/SSD/src/github.com/myobie/smalltalk/bin/st',
        },
      },
    });
    const r = rewriteMcpJson(input);
    expect(r.changed).toBe(false);
  });

  it('unfamiliar command path is left alone (no false positives)', () => {
    const input = JSON.stringify({
      mcpServers: {
        coord: { command: '/usr/local/bin/some-other-thing' },
      },
    });
    const r = rewriteMcpJson(input);
    // The key IS renamed even though the command isn't touched —
    // that's an intentional two-independent-passes shape.
    expect(r.changed).toBe(true);
    expect(
      r.actions.some((a) => a.includes('renamed mcpServers.coord → mcpServers.st'))
    ).toBe(true);
    expect(r.actions.some((a) => a.startsWith('mcpServers.st.command:'))).toBe(false);
    const parsed = JSON.parse(r.text);
    expect(parsed.mcpServers.st.command).toBe('/usr/local/bin/some-other-thing');
  });
});

describe('rewriteMcpJson — env key renames', () => {
  it('renames COORD_IDENTITY → ST_AGENT', () => {
    const input = JSON.stringify({
      mcpServers: {
        coord: {
          command: 'x',
          env: { COORD_IDENTITY: 'alice', COORD_ROOT: '/tmp/root' },
        },
      },
    });
    const r = rewriteMcpJson(input);
    const parsed = JSON.parse(r.text);
    expect(parsed.mcpServers.st.env).toEqual({
      ST_AGENT: 'alice',
      ST_ROOT: '/tmp/root',
    });
  });

  it('renames COORD_CONFIG → ST_CONFIG', () => {
    const input = JSON.stringify({
      mcpServers: {
        coord: {
          command: 'x',
          env: { COORD_CONFIG: '/tmp/cfg' },
        },
      },
    });
    const r = rewriteMcpJson(input);
    const parsed = JSON.parse(r.text);
    expect(parsed.mcpServers.st.env.ST_CONFIG).toBe('/tmp/cfg');
    expect(parsed.mcpServers.st.env.COORD_CONFIG).toBeUndefined();
  });

  it('drops redundant COORD_IDENTITY when ST_AGENT is already set', () => {
    const input = JSON.stringify({
      mcpServers: {
        coord: {
          command: 'x',
          env: {
            ST_AGENT: 'alice',
            COORD_IDENTITY: 'alice',
          },
        },
      },
    });
    const r = rewriteMcpJson(input);
    const parsed = JSON.parse(r.text);
    expect(parsed.mcpServers.st.env).toEqual({ ST_AGENT: 'alice' });
    expect(
      r.actions.some((a) => a.includes('dropped redundant COORD_IDENTITY'))
    ).toBe(true);
  });

  it('drops legacy ST_IDENTITY when ST_AGENT is already set', () => {
    const input = JSON.stringify({
      mcpServers: {
        coord: {
          command: 'x',
          env: { ST_AGENT: 'alice', ST_IDENTITY: 'alice' },
        },
      },
    });
    const r = rewriteMcpJson(input);
    const parsed = JSON.parse(r.text);
    expect(parsed.mcpServers.st.env).toEqual({ ST_AGENT: 'alice' });
  });
});

describe('rewriteMcpJson — malformed and edge inputs', () => {
  it('throws on unparseable JSON', () => {
    expect(() => rewriteMcpJson('{ not json')).toThrow();
  });

  it('leaves a well-formed JSON with no mcpServers key alone', () => {
    const input = JSON.stringify({ somethingElse: {} }, null, 2);
    const r = rewriteMcpJson(input);
    expect(r.changed).toBe(false);
  });

  it('leaves a JSON with an array-shaped mcpServers alone', () => {
    const input = JSON.stringify({ mcpServers: [] });
    const r = rewriteMcpJson(input);
    expect(r.changed).toBe(false);
  });

  it('does not preserve trailing newline when input lacked one', () => {
    // Realistic test: verify the writer is honest about the input
    // shape rather than sneaking in a newline.
    const input = JSON.stringify({
      mcpServers: {
        coord: { command: '/Volumes/SSD/src/github.com/myobie/coord/bin/coord' },
      },
    });
    const r = rewriteMcpJson(input);
    expect(r.text.endsWith('\n')).toBe(false);
  });

  it('preserves other unrelated mcpServers entries verbatim', () => {
    const input = JSON.stringify({
      mcpServers: {
        coord: { command: '/Volumes/SSD/src/github.com/myobie/coord/bin/coord' },
        other: { command: '/usr/local/bin/other', args: ['--x'] },
      },
    });
    const r = rewriteMcpJson(input);
    const parsed = JSON.parse(r.text);
    expect(parsed.mcpServers.other).toEqual({
      command: '/usr/local/bin/other',
      args: ['--x'],
    });
  });
});
