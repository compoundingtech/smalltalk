// tests/unit/resilience.test.ts — read-side resilience pass per
// brief-016 task 3. The rules:
//   1. Missing identity folder on read → empty/null, not error.
//   2. Malformed frontmatter → still parseable, body still readable,
//      parsed fields are null (no crash).
//   3. Broken in-reply-to chain → tolerated.
//   4. Partial-write file (0-byte / mid-rsync) → not a crash; the
//      iterating verbs surface what they can.
//   5. Malformed status file → treated as offline.
//   6. Concurrent status writes → no corruption.
//   7. Disk-full / write failure → clear error, no partial state.
//   8. Permission errors → no crash; surface empties.

import {
  chmodSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { cmdLs } from '../../src/commands/ls.ts';
import { cmdMembers } from '../../src/commands/agents.ts';
import { cmdRead } from '../../src/commands/read.ts';
import { cmdStatus } from '../../src/commands/status.ts';
import { cmdThread } from '../../src/commands/thread.ts';

let scratch: string;
let coordRoot: string;
const chmodRestores: Array<{ path: string; mode: number }> = [];

beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), 'coord-resilience-test-'));
  coordRoot = join(scratch, 'coord');
  mkdirSync(coordRoot, { recursive: true });
});
afterEach(() => {
  // Restore any permissions we changed so rmSync can clean up.
  for (const { path, mode } of chmodRestores) {
    try {
      chmodSync(path, mode);
    } catch {
      // best-effort
    }
  }
  chmodRestores.length = 0;
  rmSync(scratch, { recursive: true, force: true });
});

function setupIdentity(id: string): void {
  mkdirSync(join(coordRoot, id, 'inbox'), { recursive: true });
  mkdirSync(join(coordRoot, id, 'archive'), { recursive: true });
}

function rememberChmod(path: string, currentMode: number): void {
  chmodRestores.push({ path, mode: currentMode });
}

// ─── Rule 1 — missing identity folder on read ──────────────────────────

describe('resilience: missing identity folder on read', () => {
  it('cmdLs <id> with only inbox/ exists → empty list, no throw', () => {
    mkdirSync(join(coordRoot, 'partial', 'inbox'), { recursive: true });
    const r = cmdLs({
      recipient: 'partial',
      env: {} as NodeJS.ProcessEnv,
      coordRoot,
    });
    expect(r.matches).toEqual([]);
  });

  it('cmdLs --archive on identity with only inbox/ → empty list, no throw', () => {
    mkdirSync(join(coordRoot, 'partial', 'inbox'), { recursive: true });
    const r = cmdLs({
      recipient: 'partial',
      archive: true,
      env: {} as NodeJS.ProcessEnv,
      coordRoot,
    });
    expect(r.matches).toEqual([]);
  });

  it('cmdMembers with no identities → empty array, no throw', () => {
    const r = cmdMembers({ coordRoot });
    expect(r.items).toEqual([]);
  });

});

// ─── Rule 2 — malformed frontmatter ────────────────────────────────────

describe('resilience: malformed frontmatter', () => {
  beforeEach(() => {
    setupIdentity('alice');
  });

  it('cmdRead tolerates unterminated frontmatter fence', () => {
    writeFileSync(
      join(coordRoot, 'alice', 'inbox', '1714826789010-aaaaaa.md'),
      '---\nfrom: bob\nsubject: x\n(no closing fence)\nbody continues\n'
    );
    expect(() =>
      cmdRead({
        recipient: 'alice',
        filename: '1714826789010-aaaaaa.md',
        env: {} as NodeJS.ProcessEnv,
        coordRoot,
      })
    ).not.toThrow();
  });

  it('cmdLs lists files with key-only frontmatter lines', () => {
    writeFileSync(
      join(coordRoot, 'alice', 'inbox', '1714826789010-aaaaaa.md'),
      '---\nfrom\nrandomKey:\n---\nbody\n'
    );
    const r = cmdLs({
      recipient: 'alice',
      env: {} as NodeJS.ProcessEnv,
      coordRoot,
    });
    expect(r.matches).toContain('1714826789010-aaaaaa.md');
  });

  it('cmdLs tolerates a file of binary-ish bytes', () => {
    writeFileSync(
      join(coordRoot, 'alice', 'inbox', '1714826789010-aaaaaa.md'),
      Buffer.from([0x00, 0xff, 0x7f, 0x01, 0x02])
    );
    expect(() =>
      cmdLs({
        recipient: 'alice',
        env: {} as NodeJS.ProcessEnv,
        coordRoot,
      })
    ).not.toThrow();
  });

  it('cmdLs --from filter with malformed frontmatter just skips non-matches', () => {
    writeFileSync(
      join(coordRoot, 'alice', 'inbox', '1714826789010-aaaaaa.md'),
      'no fence at all\n'
    );
    writeFileSync(
      join(coordRoot, 'alice', 'inbox', '1714826789020-bbbbbb.md'),
      '---\nfrom: bob\n---\nb\n'
    );
    const r = cmdLs({
      recipient: 'alice',
      fromFilter: 'bob',
      env: {} as NodeJS.ProcessEnv,
      coordRoot,
    });
    expect(r.matches).toEqual(['1714826789020-bbbbbb.md']);
  });
});

// ─── Rule 3 — broken in-reply-to chain ─────────────────────────────────

describe('resilience: broken in-reply-to chain', () => {
  it('cmdThread tolerates an in-reply-to filename that doesn\'t exist', () => {
    setupIdentity('alice');
    writeFileSync(
      join(coordRoot, 'alice', 'inbox', '1714826789020-bbbbbb.md'),
      '---\nfrom: bob\nin-reply-to: 1714826789010-ghost1.md\n---\nreply\n'
    );
    // Should NOT throw; the walk yields what it can.
    expect(() =>
      cmdThread({
        recipient: 'alice',
        filename: '1714826789020-bbbbbb.md',
        env: {} as NodeJS.ProcessEnv,
        coordRoot,
      })
    ).not.toThrow();
  });
});

// ─── Rule 4 — partial-write files ──────────────────────────────────────

describe('resilience: partial-write files', () => {
  beforeEach(() => {
    setupIdentity('alice');
  });

  it('0-byte inbox file does not crash cmdLs', () => {
    writeFileSync(join(coordRoot, 'alice', 'inbox', '1714826789010-aaaaaa.md'), '');
    const r = cmdLs({
      recipient: 'alice',
      env: {} as NodeJS.ProcessEnv,
      coordRoot,
    });
    expect(r.matches).toContain('1714826789010-aaaaaa.md');
  });

  it('0-byte inbox file does not crash cmdRead', () => {
    writeFileSync(join(coordRoot, 'alice', 'inbox', '1714826789010-aaaaaa.md'), '');
    expect(() =>
      cmdRead({
        recipient: 'alice',
        filename: '1714826789010-aaaaaa.md',
        env: {} as NodeJS.ProcessEnv,
        coordRoot,
      })
    ).not.toThrow();
  });

  it('half-written frontmatter (missing closing fence) does not crash cmdLs --from', () => {
    // Simulates an rsync mid-transfer that copied half the bytes.
    writeFileSync(
      join(coordRoot, 'alice', 'inbox', '1714826789010-aaaaaa.md'),
      '---\nfrom: bob\nsub'
    );
    expect(() =>
      cmdLs({
        recipient: 'alice',
        fromFilter: 'bob',
        env: {} as NodeJS.ProcessEnv,
        coordRoot,
      })
    ).not.toThrow();
  });
});

// ─── Rule 5 — malformed status file ────────────────────────────────────

describe('resilience: malformed status file', () => {
  it('garbage status file → cmdStatus reports offline', () => {
    setupIdentity('alice');
    writeFileSync(join(coordRoot, 'alice', 'status'), '???\n');
    const r = cmdStatus({
      recipient: 'alice',
      env: {} as NodeJS.ProcessEnv,
      coordRoot,
    });
    expect(r.mode).toBe('get');
    if (r.mode === 'get') expect(r.state).toBe('offline');
  });

  it('multiline status file → first line wins; garbage normalizes to offline', () => {
    setupIdentity('alice');
    writeFileSync(
      join(coordRoot, 'alice', 'status'),
      'random text\navailable\n'
    );
    const r = cmdStatus({
      recipient: 'alice',
      env: {} as NodeJS.ProcessEnv,
      coordRoot,
    });
    if (r.mode === 'get') expect(r.state).toBe('offline');
  });

  it('cmdMembers --status filter routes malformed-status to offline', () => {
    setupIdentity('alice');
    setupIdentity('bob');
    writeFileSync(join(coordRoot, 'alice', 'status'), 'JUNK!!!\n');
    writeFileSync(join(coordRoot, 'bob', 'status'), 'available\n');
    const offline = cmdMembers({ status: 'offline', coordRoot });
    expect(offline.items.map((m) => m.identity)).toEqual(['alice']);
  });
});

// ─── Rule 6 — concurrent status writes ─────────────────────────────────

describe('resilience: concurrent status writes', () => {
  it('two sequential set calls leave a well-formed status file', () => {
    setupIdentity('alice');
    cmdStatus({
      recipient: 'alice',
      setState: 'busy',
      env: {} as NodeJS.ProcessEnv,
      coordRoot,
    });
    cmdStatus({
      recipient: 'alice',
      setState: 'available',
      env: {} as NodeJS.ProcessEnv,
      coordRoot,
    });
    // File ends up with exactly the last state, plus newline.
    const text = readFileSync(join(coordRoot, 'alice', 'status'), 'utf8');
    expect(text.trim()).toBe('available');
  });
});

// ─── Rule 8 — permission-blocked reads degrade gracefully ──────────────

describe('resilience: permission errors on read', () => {
  it('chmod 000 inbox dir → cmdLs returns empty (does not crash)', () => {
    setupIdentity('alice');
    writeFileSync(
      join(coordRoot, 'alice', 'inbox', '1714826789010-aaaaaa.md'),
      '---\nfrom: bob\n---\nx\n'
    );
    const inboxPath = join(coordRoot, 'alice', 'inbox');
    rememberChmod(inboxPath, 0o755);
    chmodSync(inboxPath, 0o000);
    const r = cmdLs({
      recipient: 'alice',
      env: {} as NodeJS.ProcessEnv,
      coordRoot,
    });
    // Reads degrade gracefully — empty list, no throw, no
    // misleading "0 messages" claim is more honest than crashing.
    expect(r.matches).toEqual([]);
  });

});
