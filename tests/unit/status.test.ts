// tests/unit/status.test.ts — coverage for cmd_status.

import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import {
  cmdStatus,
  isValidState,
  type StatusInput,
} from '../../src/commands/status.ts';

let scratch: string;
let coordRoot: string;

beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), 'coord-status-test-'));
  coordRoot = join(scratch, 'coord');
  mkdirSync(coordRoot, { recursive: true });
});
afterEach(() => {
  rmSync(scratch, { recursive: true, force: true });
});

function setupIdentity(id: string): void {
  mkdirSync(join(coordRoot, id, 'inbox'), { recursive: true });
  mkdirSync(join(coordRoot, id, 'archive'), { recursive: true });
}

function input(overrides: Partial<StatusInput> = {}): StatusInput {
  return {
    env: { COORD_IDENTITY: 'alice' } as NodeJS.ProcessEnv,
    coordRoot,
    ...overrides,
  };
}

// ─── isValidState ───────────────────────────────────────────────────────

describe('isValidState', () => {
  // `unknown` IS a known state — it's a derived value surfaced by the
  // mtime-staleness rule. What it's not is *settable* by the user; that
  // lives behind isSettableState in status.ts. See brief-022.
  // brief-029: `away` joins the settable set.
  it.each(['offline', 'available', 'busy', 'away', 'dnd', 'unknown'])(
    'accepts %s',
    (s) => {
      expect(isValidState(s)).toBe(true);
    }
  );
  it.each(['', 'AVAILABLE', 'busy ', 'foo'])('rejects %s', (s) => {
    expect(isValidState(s)).toBe(false);
  });
});

// ─── form 1: get my status ──────────────────────────────────────────────

describe('cmdStatus — form 1 (get COORD_IDENTITY)', () => {
  it('returns offline when no status file exists', () => {
    setupIdentity('alice');
    const r = cmdStatus(input());
    expect(r).toEqual({ mode: 'get', identity: 'alice', state: 'offline' });
  });

  it('returns the stored state when file exists', () => {
    setupIdentity('alice');
    writeFileSync(join(coordRoot, 'alice', 'status'), 'busy\n');
    const r = cmdStatus(input());
    expect(r).toEqual({ mode: 'get', identity: 'alice', state: 'busy' });
  });

  it.each(['offline', 'available', 'busy', 'dnd'])(
    'reads back stored state %s',
    (s) => {
      setupIdentity('alice');
      writeFileSync(join(coordRoot, 'alice', 'status'), `${s}\n`);
      const r = cmdStatus(input());
      expect(r).toMatchObject({ mode: 'get', state: s });
    }
  );
});

// ─── form 2: get someone else's status ──────────────────────────────────

describe('cmdStatus — form 2 (get <identity>)', () => {
  it('reads bob status with explicit positional', () => {
    setupIdentity('alice');
    setupIdentity('bob');
    writeFileSync(join(coordRoot, 'bob', 'status'), 'dnd\n');
    const r = cmdStatus(input({ recipient: 'bob' }));
    expect(r).toEqual({ mode: 'get', identity: 'bob', state: 'dnd' });
  });

  it('positional wins over COORD_IDENTITY env', () => {
    setupIdentity('alice');
    setupIdentity('bob');
    writeFileSync(join(coordRoot, 'alice', 'status'), 'busy\n');
    writeFileSync(join(coordRoot, 'bob', 'status'), 'dnd\n');
    const r = cmdStatus(input({ recipient: 'bob' }));
    expect(r.state).toBe('dnd');
  });

  it('errors with mkdir hint when identity folder is missing', () => {
    expect(() => cmdStatus(input({ recipient: 'ghost' }))).toThrowError(
      /(agent|identity) folder missing/
    );
  });
});

// ─── form 3: set my status ──────────────────────────────────────────────

describe('cmdStatus — form 3 (--set <state>)', () => {
  it.each(['offline', 'available', 'busy', 'away', 'dnd'])(
    'writes %s for COORD_IDENTITY',
    (s) => {
      setupIdentity('alice');
      const r = cmdStatus(input({ setState: s }));
      expect(r).toMatchObject({
        mode: 'set',
        identity: 'alice',
        state: s,
        written: s,
      });
      expect(readFileSync(join(coordRoot, 'alice', 'status'), 'utf8')).toBe(
        `${s}\n`
      );
    }
  );

  it('rejects invalid state with the canonical error message', () => {
    setupIdentity('alice');
    expect(() => cmdStatus(input({ setState: 'urgent' }))).toThrowError(
      /state must be one of: offline, available, busy, away, dnd/
    );
  });

  it('rejects empty-string state via --set', () => {
    setupIdentity('alice');
    expect(() => cmdStatus(input({ setState: '' }))).toThrowError(
      /state must be one of/
    );
  });

  it('roundtrip: set then get', () => {
    setupIdentity('alice');
    cmdStatus(input({ setState: 'available' }));
    expect(cmdStatus(input()).state).toBe('available');
  });
});

// ─── form 4: set someone else's status ──────────────────────────────────

describe('cmdStatus — form 4 (<identity> --set <state>)', () => {
  it('writes the state for the explicit identity, not COORD_IDENTITY', () => {
    setupIdentity('alice');
    setupIdentity('bob');
    cmdStatus(input({ recipient: 'bob', setState: 'busy' }));
    expect(readFileSync(join(coordRoot, 'bob', 'status'), 'utf8')).toBe(
      'busy\n'
    );
    // alice untouched.
    expect(existsSync(join(coordRoot, 'alice', 'status'))).toBe(false);
  });

  it('errors when the target identity has no folder', () => {
    setupIdentity('alice');
    expect(() =>
      cmdStatus(input({ recipient: 'carol', setState: 'busy' }))
    ).toThrowError(/(agent|identity) folder missing for carol/);
  });
});

// ─── invalid file content normalizes to offline ────────────────────────

describe('cmdStatus — read normalizes invalid file content', () => {
  it('garbage content → offline', () => {
    setupIdentity('alice');
    writeFileSync(join(coordRoot, 'alice', 'status'), 'garbage\n');
    expect(cmdStatus(input()).state).toBe('offline');
  });

  it('empty file → offline', () => {
    setupIdentity('alice');
    writeFileSync(join(coordRoot, 'alice', 'status'), '');
    expect(cmdStatus(input()).state).toBe('offline');
  });

  it('uppercase value → offline (normalize)', () => {
    setupIdentity('alice');
    writeFileSync(join(coordRoot, 'alice', 'status'), 'BUSY\n');
    expect(cmdStatus(input()).state).toBe('offline');
  });

  it('extra whitespace around a valid state still parses', () => {
    setupIdentity('alice');
    writeFileSync(join(coordRoot, 'alice', 'status'), '  busy  \n');
    expect(cmdStatus(input()).state).toBe('busy');
  });

  it('multi-line content: only first line considered', () => {
    setupIdentity('alice');
    writeFileSync(
      join(coordRoot, 'alice', 'status'),
      'busy\nextra\nlines\n'
    );
    expect(cmdStatus(input()).state).toBe('busy');
  });

  it('multi-line content with garbage first line → offline', () => {
    setupIdentity('alice');
    writeFileSync(
      join(coordRoot, 'alice', 'status'),
      'garbage\nbusy\n'
    );
    expect(cmdStatus(input()).state).toBe('offline');
  });
});

// ─── identity-required error ────────────────────────────────────────────

describe('cmdStatus — identity-required error', () => {
  it('errors mentioning COORD_IDENTITY when neither positional nor env', () => {
    expect(() =>
      cmdStatus({ env: {} as NodeJS.ProcessEnv, coordRoot })
    ).toThrowError(/COORD_IDENTITY/);
  });
});

// ─── reserved identity rejection ────────────────────────────────────────

describe('cmdStatus — reserved identity names rejected', () => {
  it('positional state-word "busy" → invalid identity (no longer a write)', () => {
    setupIdentity('alice');
    expect(() => cmdStatus(input({ recipient: 'busy' }))).toThrowError(
      /invalid (agent name|identity)/
    );
  });

  it('positional "offline" likewise', () => {
    expect(() => cmdStatus(input({ recipient: 'offline' }))).toThrowError(
      /invalid (agent name|identity)/
    );
  });
});
