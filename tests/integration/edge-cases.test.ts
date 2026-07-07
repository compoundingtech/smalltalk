// tests/integration/edge-cases.test.ts — concurrent + edge-case sends.
//
// Stresses the binary against the corner cases the unit tests can't easily
// hit: real stdin pipes, real concurrent invocations, real archive trim
// against a real filesystem.

import { spawn } from 'node:child_process';
import {
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import {
  COORD_BIN,
  cleanupRoot,
  listArchive,
  listInbox,
  mkIdentity,
  mkRoot,
  rsyncAvailable,
  runCoord,
} from './helpers.ts';

let root: string;

beforeEach(() => {
  root = mkRoot();
  mkIdentity(root, 'alice');
  mkIdentity(root, 'bob');
});
afterEach(() => {
  cleanupRoot(root);
});

// ─── 1MB body roundtrip ────────────────────────────────────────────────

describe('edge-cases — 1MB body', () => {
  it('survives send + read --raw byte-for-byte', () => {
    // Use printable bytes so utf-8 → buffer roundtrip is identity-stable.
    const body = 'a'.repeat(1024 * 1024); // 1 MiB
    const send = runCoord(['message', 'send', 'bob', '--from', 'alice'], {
      stRoot: root,
      coordIdentity: 'alice',
      stdin: body,
    });
    expect(send.exitCode).toBe(0);
    const filename = send.stdout.trim();
    const read = runCoord(['message', 'read', 'bob', filename, '--raw'], {
      stRoot: root,
      coordIdentity: 'alice',
    });
    expect(read.exitCode).toBe(0);
    // The raw output is the entire file (frontmatter + body + trailing
    // newline). Byte-for-byte equal to what's on disk.
    const onDisk = readFileSync(join(root, 'bob', 'inbox', filename), 'utf8');
    expect(read.stdout.length).toBe(onDisk.length);
    expect(read.stdout).toBe(onDisk);
    expect(read.stdout.endsWith(`${body}\n`)).toBe(true);
  });

  it('sync to a peer also preserves a 1MB body byte-for-byte', () => {
    if (!rsyncAvailable()) return;
    const peer = mkRoot();
    try {
      mkIdentity(peer, 'bob');
      const body = 'x'.repeat(1024 * 1024);
      const send = runCoord(['message', 'send', 'bob', '--from', 'alice'], {
        stRoot: root,
        coordIdentity: 'alice',
        stdin: body,
      });
      const filename = send.stdout.trim();
      runCoord(['sync', 'push', `local:${peer}`], {
        stRoot: root,
        coordIdentity: 'alice',
      });
      const local = readFileSync(join(root, 'bob', 'inbox', filename));
      const remote = readFileSync(join(peer, 'bob', 'inbox', filename));
      expect(local.equals(remote)).toBe(true);
    } finally {
      cleanupRoot(peer);
    }
  });
});

// ─── Concurrent sends ──────────────────────────────────────────────────

describe('edge-cases — concurrent sends', () => {
  it('two parallel sends produce two distinct files (rand6 disambiguates)', async () => {
    // Spawn both sends in parallel via child_process.spawn so they really
    // run concurrently (unlike spawnSync's serialized behavior).
    const send = (body: string): Promise<string> =>
      new Promise((resolve, reject) => {
        const proc = spawn(
          COORD_BIN,
          ['message', 'send', 'bob', '--from', 'alice'],
          {
            env: {
              PATH: process.env.PATH,
              HOME: process.env.HOME,
              ST_ROOT: root,
              ST_AGENT: 'alice',
            },
            stdio: ['pipe', 'pipe', 'pipe'],
          }
        );
        let out = '';
        let err = '';
        proc.stdout.on('data', (d) => {
          out += d.toString();
        });
        proc.stderr.on('data', (d) => {
          err += d.toString();
        });
        proc.on('close', (code) => {
          if (code === 0) resolve(out.trim());
          else reject(new Error(`send exited ${code}: ${err}`));
        });
        proc.stdin.end(body);
      });

    const [a, b] = await Promise.all([send('first'), send('second')]);
    expect(a).not.toBe(b);
    const ls = listInbox(root, 'bob');
    expect(ls).toHaveLength(2);
    expect(ls).toContain(a);
    expect(ls).toContain(b);
  });

  it('30 rapid serial sends produce 30 distinct files', () => {
    for (let i = 0; i < 30; i++) {
      const r = runCoord(['message', 'send', 'bob', '--from', 'alice'], {
        stRoot: root,
        coordIdentity: 'alice',
        stdin: `msg ${i}`,
      });
      expect(r.exitCode).toBe(0);
    }
    expect(listInbox(root, 'bob')).toHaveLength(30);
  });
});

// ─── Empty body ────────────────────────────────────────────────────────

describe('edge-cases — empty body', () => {
  it('errors with non-zero exit, no file is created', () => {
    const before = listInbox(root, 'bob');
    const r = runCoord(['message', 'send', 'bob', '--from', 'alice'], {
      stRoot: root,
      coordIdentity: 'alice',
      stdin: '',
    });
    expect(r.exitCode).not.toBe(0);
    expect(r.stderr).toMatch(/message body is empty/);
    expect(listInbox(root, 'bob')).toEqual(before);
  });
});

// ─── --in-reply-to to a non-existent file ──────────────────────────────

describe('edge-cases — --in-reply-to', () => {
  it('accepts a valid filename even when the target file does not exist locally', () => {
    // The send-time validator only checks grammar; thread walker tolerates
    // broken parents. So `--in-reply-to <unseen>.md` succeeds.
    const phantom = '1714826789999-zzzzzz.md';
    const r = runCoord(
      ['message', 'send', 'bob', '--from', 'alice', '--in-reply-to', phantom],
      {
        stRoot: root,
        coordIdentity: 'alice',
        stdin: 'reply to nothing',
      }
    );
    expect(r.exitCode).toBe(0);
    const filename = r.stdout.trim();
    const text = readFileSync(join(root, 'bob', 'inbox', filename), 'utf8');
    expect(text).toContain(`in-reply-to: ${phantom}`);
  });

  it('rejects a value that does not match the filename grammar', () => {
    const r = runCoord(
      ['message', 'send', 'bob', '--from', 'alice', '--in-reply-to', 'garbage'],
      {
        stRoot: root,
        coordIdentity: 'alice',
        stdin: 'body',
      }
    );
    expect(r.exitCode).not.toBe(0);
    expect(r.stderr).toContain('invalid filename');
  });
});

// ─── Half-formed identity (folder exists but lacks inbox/) ─────────────

describe('edge-cases — half-formed sender identity', () => {
  it('coord send --from <half> errors clearly', () => {
    // alice's folder has inbox/archive but we'll use a different name.
    mkdirSync(join(root, 'half'), { recursive: true });
    // Note: only the parent dir exists — no inbox/archive subdirs.
    const r = runCoord(['message', 'send', 'bob', '--from', 'half'], {
      stRoot: root,
      coordIdentity: 'alice',
      stdin: 'body',
    });
    expect(r.exitCode).not.toBe(0);
    // Post-rename: error message says "agent folder missing".
    expect(r.stderr).toMatch(/(agent|identity) folder missing for half/);
  });
});

// ─── archive trim --dry-run preserves the filesystem ──────────────────

describe('edge-cases — archive trim --dry-run', () => {
  it('lists victims to stdout, deletes nothing', () => {
    // Pre-populate alice's archive with five chronologically-named files.
    for (const ts of [
      '1714826789010',
      '1714826789020',
      '1714826789030',
      '1714826789040',
      '1714826789050',
    ]) {
      writeFileSync(
        join(root, 'alice', 'archive', `${ts}-aaaaaa.md`),
        `---\nfrom: bob\n---\n${ts}\n`
      );
    }
    const before = listArchive(root, 'alice');
    expect(before).toHaveLength(5);

    const r = runCoord(
      ['message', 'archive', 'trim', 'alice', '--keep-last', '2', '--dry-run'],
      { stRoot: root, coordIdentity: 'alice' }
    );
    expect(r.exitCode).toBe(0);

    // Stdout lists the 3 oldest victims; nothing was deleted.
    const stdoutLines = r.stdout.trim().split('\n').filter(Boolean);
    expect(stdoutLines).toEqual([
      '1714826789010-aaaaaa.md',
      '1714826789020-aaaaaa.md',
      '1714826789030-aaaaaa.md',
    ]);
    expect(r.stderr).toContain(
      'would trim 3 files (dry run; nothing deleted)'
    );
    expect(listArchive(root, 'alice')).toEqual(before);
  });

  it('without --dry-run deletes the same victims', () => {
    for (const ts of [
      '1714826789010',
      '1714826789020',
      '1714826789030',
      '1714826789040',
      '1714826789050',
    ]) {
      writeFileSync(
        join(root, 'alice', 'archive', `${ts}-aaaaaa.md`),
        'x'
      );
    }
    const r = runCoord(
      ['message', 'archive', 'trim', 'alice', '--keep-last', '2'],
      { stRoot: root, coordIdentity: 'alice' }
    );
    expect(r.exitCode).toBe(0);
    expect(r.stderr).toContain('trimmed 3 files');
    const remaining = listArchive(root, 'alice');
    expect(remaining).toEqual([
      '1714826789040-aaaaaa.md',
      '1714826789050-aaaaaa.md',
    ]);
  });
});

// ─── Corrupted frontmatter on read ────────────────────────────────────

describe('edge-cases — corrupted/missing frontmatter on read', () => {
  it('coord read on a file with no frontmatter dumps the body, marked untyped', () => {
    const filename = '1714826789010-aaaaaa.md';
    writeFileSync(join(root, 'alice', 'inbox', filename), 'just text\n');
    const r = runCoord(['message', 'read', 'alice', filename], {
      stRoot: root,
      coordIdentity: 'alice',
    });
    expect(r.exitCode).toBe(0);
    expect(r.stdout).toBe('just text\n');
    expect(r.stderr).toContain('(untyped: no frontmatter)');
  });

  it('coord ls --from on a file with malformed frontmatter silently excludes it', () => {
    const goodFile = '1714826789010-aaaaaa.md';
    writeFileSync(
      join(root, 'alice', 'inbox', goodFile),
      '---\nfrom: bob\n---\nbody\n'
    );
    const badFile = '1714826789020-bbbbbb.md';
    writeFileSync(join(root, 'alice', 'inbox', badFile), 'no fences\n');

    const r = runCoord(['message', 'ls', 'alice', '--from', 'bob'], {
      stRoot: root,
      coordIdentity: 'alice',
    });
    expect(r.exitCode).toBe(0);
    expect(r.stdout.trim().split('\n')).toEqual([goodFile]);
  });
});
