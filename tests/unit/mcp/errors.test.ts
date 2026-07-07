// tests/unit/mcp/errors.test.ts — coordErrorToToolResult mapping.
//
// Every CoordError subclass surfaces with its stable `code`, the
// content[0].text prefixed `<CODE>:`, and structuredContent carrying
// { code, message, details }.

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
} from '../../../src/errors.ts';
import {
  buildToolResult,
  coordErrorToToolResult,
  withErrorMapping,
} from '../../../src/mcp/error-mapping.ts';

describe('coordErrorToToolResult — every CoordError subclass round-trips', () => {
  it.each([
    [new IdentityRequiredError(), 'IDENTITY_REQUIRED'],
    [new IdentityNotHostedError('ghost'), 'IDENTITY_NOT_HOSTED'],
    [new InvalidIdentityError('INVALID'), 'INVALID_IDENTITY'],
    [new InvalidFilenameError('garbage'), 'INVALID_FILENAME'],
    [new InvalidStateError('foo'), 'INVALID_STATE'],
    [new InvalidPriorityError('urgent'), 'INVALID_PRIORITY'],
    [new InvalidDurationError('xy'), 'INVALID_DURATION'],
    [new MessageNotFoundError('bob', 'X.md'), 'MESSAGE_NOT_FOUND'],
    [
      new SyncFailedError('push', 23, 'connection refused', 'rsync push failed: a -> b'),
      'SYNC_FAILED',
    ],
    [new PeersConfigMissingError('/cfg/peers.yaml'), 'PEERS_CONFIG_MISSING'],
    [new PeersConfigInvalidError('/cfg', 'no peers found'), 'PEERS_CONFIG_INVALID'],
    [new EmptyBodyError(), 'EMPTY_BODY'],
    [new ArchiveConflictError('bob', 'X.md'), 'ARCHIVE_CONFLICT'],
  ])('maps %s → code=%s', (err, code) => {
    const r = coordErrorToToolResult(err);
    expect(r.isError).toBe(true);
    expect(r.content[0]).toMatchObject({ type: 'text' });
    const text = (r.content[0] as { text: string }).text;
    expect(text).toMatch(new RegExp(`^${code}:`));
    expect(r._meta?.['coord/error']).toMatchObject({
      code,
      message: (err as CoordError).message,
    });
    expect(r.structuredContent).toBeUndefined();
  });
});

describe('coordErrorToToolResult — non-CoordError fallback', () => {
  it('plain Error → INTERNAL_ERROR with the message', () => {
    const r = coordErrorToToolResult(new Error('something bad'));
    expect(r.isError).toBe(true);
    expect((r.content[0] as { text: string }).text).toBe(
      'INTERNAL_ERROR: something bad'
    );
    expect(r._meta?.['coord/error']).toEqual({
      code: 'INTERNAL_ERROR',
      message: 'something bad',
    });
  });

  it('non-Error throw (string) → INTERNAL_ERROR with the stringified value', () => {
    const r = coordErrorToToolResult('boom');
    expect(r.isError).toBe(true);
    expect((r.content[0] as { text: string }).text).toBe(
      'INTERNAL_ERROR: boom'
    );
    expect(r._meta?.['coord/error']).toEqual({
      code: 'INTERNAL_ERROR',
      message: 'boom',
    });
  });

  it('CoordError subclasses preserve the details payload in _meta', () => {
    const r = coordErrorToToolResult(new IdentityNotHostedError('ghost'));
    expect(r._meta?.['coord/error']).toMatchObject({
      code: 'IDENTITY_NOT_HOSTED',
      details: { identity: 'ghost' },
    });
  });

  it('CoordError without details omits the details key', () => {
    const r = coordErrorToToolResult(new IdentityRequiredError());
    expect(r._meta?.['coord/error']).toEqual({
      code: 'IDENTITY_REQUIRED',
      message: expect.stringContaining('ST_AGENT'),
    });
  });

  it('error responses do NOT set structuredContent (would fail outputSchema)', () => {
    const r = coordErrorToToolResult(new IdentityRequiredError());
    expect(r.structuredContent).toBeUndefined();
  });
});

describe('buildToolResult — happy-path constructor', () => {
  it('summary-only → content text, no structuredContent, no isError', () => {
    const r = buildToolResult({ summary: 'archived' });
    expect(r.isError).toBeUndefined();
    expect(r.content).toEqual([{ type: 'text', text: 'archived' }]);
    expect(r.structuredContent).toBeUndefined();
  });

  it('with value → structuredContent populated', () => {
    const r = buildToolResult({
      summary: 'sent: 1714826789012-x9k4mz.md',
      value: { filename: '1714826789012-x9k4mz.md' },
    });
    expect(r.content).toEqual([
      { type: 'text', text: 'sent: 1714826789012-x9k4mz.md' },
    ]);
    expect(r.structuredContent).toEqual({
      filename: '1714826789012-x9k4mz.md',
    });
  });
});

describe('withErrorMapping — wraps async tool bodies', () => {
  it('returns the body result on success', async () => {
    const r = await withErrorMapping(async () =>
      buildToolResult({ summary: 'ok' })
    );
    expect(r.isError).toBeUndefined();
    expect(r.content[0]).toMatchObject({ text: 'ok' });
  });

  it('catches CoordError and maps to a structured error response', async () => {
    const r = await withErrorMapping(async () => {
      throw new EmptyBodyError();
    });
    expect(r.isError).toBe(true);
    expect((r.content[0] as { text: string }).text).toMatch(/^EMPTY_BODY:/);
    expect(r._meta?.['coord/error']).toMatchObject({ code: 'EMPTY_BODY' });
  });

  it('catches plain Error → INTERNAL_ERROR', async () => {
    const r = await withErrorMapping(async () => {
      throw new Error('oops');
    });
    expect(r.isError).toBe(true);
    expect(r._meta?.['coord/error']).toMatchObject({
      code: 'INTERNAL_ERROR',
      message: 'oops',
    });
  });

  it('non-Error throws are coerced via String()', async () => {
    const r = await withErrorMapping(async () => {
      throw 42;
    });
    expect(r.isError).toBe(true);
    expect(r._meta?.['coord/error']).toMatchObject({
      code: 'INTERNAL_ERROR',
      message: '42',
    });
  });
});
