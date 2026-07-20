// tests/unit/exports.test.ts — sanity-check the public surface.
//
// Imports every documented export from `src/index.ts` (root subpath of
// the package) plus the `./errors` and `./types` subpaths, and asserts
// none is `undefined`. This catches accidental drops, name typos in the
// re-export blocks, and broken file paths in the exports map.

import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { describe, expect, it } from 'vitest';

import * as index from '../../src/index.ts';
import * as errors from '../../src/errors.ts';
import * as types from '../../src/types.ts';

const REPO_ROOT = fileURLToPath(new URL('../..', import.meta.url));
const pkg = JSON.parse(
  readFileSync(join(REPO_ROOT, 'package.json'), 'utf8')
) as {
  name: string;
  type: string;
  main: string;
  types: string;
  exports: Record<string, Record<string, string>>;
  bin: Record<string, string>;
};

describe('package.json exports map', () => {
  it('declares the package as ESM', () => {
    expect(pkg.type).toBe('module');
  });

  it('keeps the canonical name', () => {
    expect(pkg.name).toBe('@compoundingtech/smalltalk');
  });

  it('main + types both point at the .ts entry (no build step required)', () => {
    expect(pkg.main).toBe('./src/index.ts');
    expect(pkg.types).toBe('./src/index.ts');
  });

  it('"." subpath resolves to ./src/index.ts for every condition', () => {
    expect(pkg.exports['.']).toEqual({
      types: './src/index.ts',
      import: './src/index.ts',
      default: './src/index.ts',
    });
  });

  it('./errors subpath resolves to ./src/errors.ts', () => {
    expect(pkg.exports['./errors']?.default).toBe('./src/errors.ts');
  });

  it('./types subpath resolves to ./src/types.ts', () => {
    expect(pkg.exports['./types']?.default).toBe('./src/types.ts');
  });

  it('declares only the `st` + `smalltalk` binaries (post-st-cutover)', () => {
    expect(pkg.bin).toEqual({
      st: './bin/st',
      smalltalk: './bin/smalltalk',
    });
  });
});

describe('root subpath — every documented export is present', () => {
  it('has the factory + handle types', () => {
    expect(typeof index.createSt).toBe('function');
    // VERSION is exported (placeholder until we publish).
    expect(typeof index.VERSION).toBe('string');
  });

  it('has the read-only bus reader', () => {
    expect(typeof index.createBusReader).toBe('function');
  });

  it.each([
    'asFilename',
    'asIdentity',
    'deriveTo',
    'deriveTs',
    'isFilename',
    'isIdentity',
    'isState',
    'parsePeer',
  ])('%s is a function', (name) => {
    expect(typeof (index as Record<string, unknown>)[name]).toBe('function');
  });

  it('STATES + PRIORITIES are arrays', () => {
    expect(Array.isArray(index.STATES)).toBe(true);
    expect(Array.isArray(index.PRIORITIES)).toBe(true);
  });

  it.each([
    'parseFrontmatter',
    'emitFrontmatter',
    'yamlQuote',
    'validFilename',
    'validIdentity',
  ])('%s is a function', (name) => {
    expect(typeof (index as Record<string, unknown>)[name]).toBe('function');
  });

  it.each([
    'StError',
    'IdentityRequiredError',
    'IdentityNotHostedError',
    'InvalidIdentityError',
    'InvalidFilenameError',
    'InvalidStateError',
    'InvalidPriorityError',
    'InvalidDurationError',
    'MessageNotFoundError',
    'SyncFailedError',
    'PeersConfigMissingError',
    'PeersConfigInvalidError',
    'EmptyBodyError',
    'ArchiveConflictError',
    'ResourceNotFoundError',
    'InvalidResourceUrlError',
  ])('%s error class is exported', (name) => {
    const ctor = (index as Record<string, unknown>)[name];
    expect(typeof ctor).toBe('function');
  });
});

describe('subpath modules expose the same symbols as the index re-exports', () => {
  it('errors module shape matches index', () => {
    for (const name of [
      'StError',
      'IdentityRequiredError',
      'EmptyBodyError',
      'SyncFailedError',
    ]) {
      expect((index as Record<string, unknown>)[name]).toBe(
        (errors as Record<string, unknown>)[name]
      );
    }
  });

  it('types module shape matches index', () => {
    for (const name of [
      'asIdentity',
      'asFilename',
      'isIdentity',
      'isFilename',
      'parsePeer',
      'STATES',
      'PRIORITIES',
    ]) {
      expect((index as Record<string, unknown>)[name]).toBe(
        (types as Record<string, unknown>)[name]
      );
    }
  });
});

describe('round-trip smoke — embedder usage', () => {
  it('createSt + asIdentity compose end to end', () => {
    // Static check: this should typecheck. The actual filesystem wiring
    // is exercised in lib.test.ts and library-embedding.test.ts.
    const id = index.asIdentity('alice');
    const handle = index.createSt({
      root: '/tmp/st-export-test-not-touched',
      identity: id,
    });
    expect(handle.root).toBe('/tmp/st-export-test-not-touched');
    expect(handle.identity).toBe('alice');
    expect(typeof handle.send).toBe('function');
    expect(typeof handle.watch).toBe('function');
    expect(typeof handle.sync.push).toBe('function');
  });

  it('StError instances pass instanceof check across the import boundary', () => {
    const err = new index.IdentityRequiredError();
    expect(err).toBeInstanceOf(index.StError);
    expect(err).toBeInstanceOf(errors.StError);
    expect(err.code).toBe('IDENTITY_REQUIRED');
  });
});
