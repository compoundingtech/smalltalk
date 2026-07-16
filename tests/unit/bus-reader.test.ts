// tests/unit/bus-reader.test.ts — the read-only bus reader export.

import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { createBusReader } from '../../src/bus-reader.ts';

let root: string;

beforeEach(() => {
  root = mkdtempSync(join(tmpdir(), 'bus-reader-test-'));
});
afterEach(() => {
  rmSync(root, { recursive: true, force: true });
});

function mkAgent(id: string, state?: string): void {
  mkdirSync(join(root, id, 'inbox'), { recursive: true });
  mkdirSync(join(root, id, 'archive'), { recursive: true });
  if (state !== undefined) writeFileSync(join(root, id, 'status'), state);
}
function putInbox(id: string, name: string, body: string): void {
  writeFileSync(join(root, id, 'inbox', name), body);
}

describe('createBusReader', () => {
  it('agents() returns the base shape { identity, status, name }', () => {
    mkAgent('alice', 'available');
    mkAgent('bob', 'busy');
    const reader = createBusReader({ root });
    const agents = reader.agents();
    expect(agents.map((a) => a.identity).sort()).toEqual(['alice', 'bob']);
    const alice = agents.find((a) => a.identity === 'alice')!;
    expect(alice.status).toBe('available');
    expect(alice).toHaveProperty('name');
    // Base shape does NOT carry enrich-only fields.
    expect(alice).not.toHaveProperty('lastActivity');
    expect(alice).not.toHaveProperty('inbox');
  });

  it('agents({ enrich: true }) adds lastActivity + inbox count', () => {
    mkAgent('alice', 'available');
    putInbox('alice', '1714826789010-aaaaaa.md', 'hi');
    putInbox('alice', '1714826789020-bbbbbb.md', 'yo');
    const reader = createBusReader({ root });
    const [alice] = reader.agents({ enrich: true });
    expect(alice!.identity).toBe('alice');
    expect(alice!.inbox).toBe(2);
    expect(typeof alice!.lastActivity).toBe('number');
  });

  it('agents({ status }) filters to a single state', () => {
    mkAgent('alice', 'available');
    mkAgent('bob', 'busy');
    const reader = createBusReader({ root });
    expect(reader.agents({ status: 'busy' }).map((a) => a.identity)).toEqual(['bob']);
  });

  it('is a stable read: repeated calls do not mutate the tree', () => {
    mkAgent('alice', 'available');
    const reader = createBusReader({ root });
    const first = reader.agents();
    const second = reader.agents();
    expect(second).toEqual(first);
  });

  it('empty / missing root → [] (no throw)', () => {
    expect(createBusReader({ root: join(root, 'nope') }).agents()).toEqual([]);
    expect(createBusReader({ root }).agents()).toEqual([]);
  });
});
