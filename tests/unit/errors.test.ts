// tests/unit/errors.test.ts — every CoordError subclass round-trips its
// code, message, and details payload, and is a real instanceof Error.

import { describe, expect, it } from 'vitest';

import {
  ArchiveConflictError,
  CoordError,
  EmptyBodyError,
  IdentityNotHostedError,
  IdentityRequiredError,
  InvalidDurationError,
  InvalidFilenameError,
  InvalidIdentityError,
  InvalidPriorityError,
  InvalidStateError,
  MessageNotFoundError,
  PeersConfigInvalidError,
  PeersConfigMissingError,
  SyncFailedError,
} from '../../src/errors.ts';

describe('CoordError (base class)', () => {
  it('preserves the message + code + details', () => {
    const err = new CoordError('TEST', 'something happened', { x: 1 });
    expect(err).toBeInstanceOf(Error);
    expect(err).toBeInstanceOf(CoordError);
    expect(err.code).toBe('TEST');
    expect(err.message).toBe('something happened');
    expect(err.details).toEqual({ x: 1 });
    expect(err.name).toBe('CoordError');
  });

  it('omits details when not provided', () => {
    const err = new CoordError('TEST', 'oops');
    expect(err.details).toBeUndefined();
  });

  it('passes the standard Error.message contract (typeof string)', () => {
    const err = new CoordError('TEST', 'msg');
    expect(typeof err.message).toBe('string');
  });
});

describe('IdentityRequiredError', () => {
  it('has stable code + canonical message', () => {
    const err = new IdentityRequiredError();
    expect(err.code).toBe('IDENTITY_REQUIRED');
    expect(err.message).toMatch(/COORD_IDENTITY/);
    expect(err).toBeInstanceOf(CoordError);
  });
});

describe('IdentityNotHostedError (alias of AgentNotHostedError)', () => {
  it('mentions the agent name + mkdir hint', () => {
    const err = new IdentityNotHostedError('ghost');
    expect(err.code).toBe('IDENTITY_NOT_HOSTED');
    expect(err.identity).toBe('ghost');
    expect(err.message).toContain('agent folder missing for ghost');
    expect(err.message).toContain('mkdir -p');
    expect(err.details).toEqual({ identity: 'ghost' });
  });
});

describe('InvalidIdentityError (alias of InvalidAgentError)', () => {
  it('round-trips the value', () => {
    const err = new InvalidIdentityError('INVALID');
    expect(err.code).toBe('INVALID_IDENTITY');
    expect(err.value).toBe('INVALID');
    expect(err.message).toBe('invalid agent name: INVALID');
  });
});

describe('InvalidFilenameError', () => {
  it('round-trips the value', () => {
    const err = new InvalidFilenameError('garbage');
    expect(err.code).toBe('INVALID_FILENAME');
    expect(err.value).toBe('garbage');
    expect(err.message).toBe('invalid filename: garbage');
  });
});

describe('MessageNotFoundError', () => {
  it('carries identity + filename in details and message', () => {
    const err = new MessageNotFoundError('bob', '1714826789012-x9k4mz.md');
    expect(err.code).toBe('MESSAGE_NOT_FOUND');
    expect(err.identity).toBe('bob');
    expect(err.filename).toBe('1714826789012-x9k4mz.md');
    expect(err.message).toContain('not found in inbox or archive');
    expect(err.message).toContain('1714826789012-x9k4mz.md');
  });
});

describe('InvalidStateError', () => {
  it('canonical message lists the settable states', () => {
    const err = new InvalidStateError('urgent');
    expect(err.code).toBe('INVALID_STATE');
    expect(err.value).toBe('urgent');
    expect(err.message).toBe(
      'state must be one of: offline, available, busy, away, dnd'
    );
  });
});

describe('InvalidPriorityError', () => {
  it('canonical message lists the three priorities', () => {
    const err = new InvalidPriorityError('urgent');
    expect(err.code).toBe('INVALID_PRIORITY');
    expect(err.value).toBe('urgent');
    expect(err.message).toBe(
      'priority must be one of: low, normal, high'
    );
  });
});

describe('InvalidDurationError', () => {
  it('mentions the value and the example', () => {
    const err = new InvalidDurationError('xy');
    expect(err.code).toBe('INVALID_DURATION');
    expect(err.value).toBe('xy');
    expect(err.message).toContain('invalid duration: xy');
    expect(err.message).toContain('90d');
  });
});

describe('SyncFailedError', () => {
  it('carries stage, exit code, stderr, and a custom message', () => {
    const err = new SyncFailedError(
      'push',
      23,
      'connection refused',
      'rsync push failed: /a -> /b'
    );
    expect(err.code).toBe('SYNC_FAILED');
    expect(err.stage).toBe('push');
    expect(err.exitCode).toBe(23);
    expect(err.stderr).toBe('connection refused');
    expect(err.message).toBe('rsync push failed: /a -> /b');
    expect(err.details).toEqual({
      stage: 'push',
      exitCode: 23,
      stderr: 'connection refused',
    });
  });

  it('handles undefined stderr', () => {
    const err = new SyncFailedError('pull', 1, undefined, 'rsync pull failed');
    expect(err.stderr).toBeUndefined();
  });
});

describe('PeersConfigMissingError', () => {
  it('carries the path', () => {
    const err = new PeersConfigMissingError('/cfg/peers.yaml');
    expect(err.code).toBe('PEERS_CONFIG_MISSING');
    expect(err.path).toBe('/cfg/peers.yaml');
    expect(err.message).toBe('no peers configured at /cfg/peers.yaml');
  });
});

describe('PeersConfigInvalidError', () => {
  it('formats reason: path', () => {
    const err = new PeersConfigInvalidError('/cfg/peers.yaml', 'no peers found in');
    expect(err.code).toBe('PEERS_CONFIG_INVALID');
    expect(err.path).toBe('/cfg/peers.yaml');
    expect(err.reason).toBe('no peers found in');
    expect(err.message).toBe('no peers found in: /cfg/peers.yaml');
  });
});

describe('EmptyBodyError', () => {
  it('has a canonical empty-body message', () => {
    const err = new EmptyBodyError();
    expect(err.code).toBe('EMPTY_BODY');
    expect(err.message).toBe('message body is empty (read from stdin)');
  });
});

describe('ArchiveConflictError', () => {
  it('mentions both the inbox and archive copies', () => {
    const err = new ArchiveConflictError('bob', 'X.md');
    expect(err.code).toBe('ARCHIVE_CONFLICT');
    expect(err.identity).toBe('bob');
    expect(err.filename).toBe('X.md');
    expect(err.message).toContain('archive/X.md');
    expect(err.message).toContain('inbox/X.md');
    expect(err.message).toContain('refuse to archive');
  });
});

describe('catch shape — JS-side `code` branching works', () => {
  it('a caller can branch on `e.code` without TypeScript narrowing', () => {
    function getMaybeCode(): string | undefined {
      try {
        throw new IdentityRequiredError();
      } catch (e: unknown) {
        if (e instanceof CoordError) return e.code;
        return undefined;
      }
    }
    expect(getMaybeCode()).toBe('IDENTITY_REQUIRED');
  });

  it('subclasses are catchable as the base CoordError', () => {
    let caught: CoordError | undefined;
    try {
      throw new InvalidStateError('foo');
    } catch (e: unknown) {
      if (e instanceof CoordError) caught = e;
    }
    expect(caught?.code).toBe('INVALID_STATE');
  });
});
