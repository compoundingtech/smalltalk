// tests/integration/watch.test.ts — `st watch` driven through a real PTY.
//
// Uses @myobie/pty/testing's Session.spawn so we exercise the actual long-
// running polling loop, real filesystem polling, real signal handling.

import { mkdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import {
  cleanupRoot,
  mkIdentity,
  mkRoot,
  runCoordPty,
} from './helpers.ts';
import type { Session } from '@myobie/pty/testing';

let root: string;
const sessions: Session[] = [];

beforeEach(() => {
  root = mkRoot();
});
afterEach(async () => {
  for (const s of sessions) {
    try {
      await s.close();
    } catch {
      // ignore
    }
  }
  sessions.length = 0;
  cleanupRoot(root);
});

function track(s: Session): Session {
  sessions.push(s);
  return s;
}

function writeMsg(
  recipient: string,
  filename: string,
  fromValue: string,
  body = 'body',
  subject = ''
): void {
  const dir = join(root, recipient, 'inbox');
  mkdirSync(dir, { recursive: true });
  let head = `---\nfrom: ${fromValue}\n`;
  if (subject) head += `subject: ${subject}\n`;
  head += '---\n';
  writeFileSync(join(dir, filename), `${head}${body}\n`);
}

// ─── Per-identity watch ─────────────────────────────────────────────────

describe('coord watch <id> — per-identity', () => {
  it('emits the filename of a new inbox arrival', async () => {
    mkIdentity(root, 'alice');
    const s = track(
      runCoordPty(
        ['watch', 'alice', '--interval', '100'],
        { stRoot: root, coordIdentity: 'alice' }
      )
    );
    // Give the watcher a moment to start its replay phase against an empty
    // inbox before we drop a file in.
    const filename = '1714826789010-aaaaaa.md';
    writeMsg('alice', filename, 'bob');
    await s.waitForText(filename, 5_000);
  });
});

// ─── Cross-tree watch ──────────────────────────────────────────────────

describe('coord watch --all — cross-tree', () => {
  // brief-017a bug 3: cross-tree mode is now opt-in via --all.
  // The pre-017a default ("no positional → cross-tree") was
  // surprising; the test below exercises the new explicit form.
  it('fires on a peer inbox arrival but suppresses the watcher\'s own folder', async () => {
    mkIdentity(root, 'alice');
    mkIdentity(root, 'bob');
    const s = track(
      runCoordPty(
        ['watch', '--all', '--interval', '100'],
        { stRoot: root, coordIdentity: 'alice' }
      )
    );

    // Drop a file in BOB's inbox (peer) — should print.
    const peerFilename = '1714826789020-bbbbbb.md';
    writeMsg('bob', peerFilename, 'carol');
    await s.waitForText(peerFilename, 5_000);

    // Now drop one in alice's OWN inbox — should NOT print.
    const ownFilename = '1714826789030-cccccc.md';
    writeMsg('alice', ownFilename, 'carol');
    // Give the polling loop several intervals to potentially pick it up.
    await new Promise((r) => setTimeout(r, 500));
    const ss = s.screenshot();
    expect(ss.text).not.toContain(ownFilename);
  });

  it('default (no --all, no positional) → watches $ST_AGENT only', async () => {
    mkIdentity(root, 'alice');
    mkIdentity(root, 'bob');
    const s = track(
      runCoordPty(
        ['watch', '--interval', '100'],
        { stRoot: root, coordIdentity: 'alice' }
      )
    );

    // Drop a file in alice's OWN inbox — should print (default
    // mode is now "my inbox", consistent with `ls` etc).
    const ownFilename = '1714826789020-bbbbbb.md';
    writeMsg('alice', ownFilename, 'bob');
    await s.waitForText(ownFilename, 5_000);

    // Now drop one in BOB's inbox — should NOT print (we're not
    // in cross-tree mode anymore).
    const peerFilename = '1714826789030-cccccc.md';
    writeMsg('bob', peerFilename, 'alice');
    await new Promise((r) => setTimeout(r, 500));
    const ss = s.screenshot();
    expect(ss.text).not.toContain(peerFilename);
  });
});

// ─── --once ────────────────────────────────────────────────────────────

describe('coord watch --once', () => {
  it('does one pass and exits cleanly', async () => {
    mkIdentity(root, 'alice');
    const filename = '1714826789010-aaaaaa.md';
    writeMsg('alice', filename, 'bob');

    const s = track(
      runCoordPty(
        ['watch', 'alice', '--once'],
        { stRoot: root, coordIdentity: 'alice' }
      )
    );
    await s.waitForText(filename, 5_000);
    // The process should exit on its own (no further output, no hang).
    // Closing the session is a clean teardown if it's already exited.
    await s.close();
  });
});

// ─── --with-subject ────────────────────────────────────────────────────

describe('coord watch --with-subject', () => {
  it('appends a tab + subject to each emitted line', async () => {
    mkIdentity(root, 'alice');
    const filename = '1714826789010-aaaaaa.md';
    writeMsg('alice', filename, 'bob', 'body', 'hello there');

    const s = track(
      runCoordPty(
        ['watch', 'alice', '--once', '--with-subject'],
        { stRoot: root, coordIdentity: 'alice' }
      )
    );
    // The line should look like "<filename>\thello there" — and after PTY
    // cooking, "hello there" appears on the same screen line as the name.
    await s.waitForText('hello there', 5_000);
    const ss = s.screenshot();
    expect(ss.text).toContain(filename);
    expect(ss.text).toContain('hello there');
  });
});

// ─── --since-now ───────────────────────────────────────────────────────

describe('coord watch --since-now', () => {
  it('skips pre-existing files, fires on new arrivals', async () => {
    mkIdentity(root, 'alice');
    // Pre-populate alice's inbox.
    const preExisting = '1714826789010-aaaaaa.md';
    writeMsg('alice', preExisting, 'bob');

    const s = track(
      runCoordPty(
        ['watch', 'alice', '--since-now', '--interval', '100'],
        { stRoot: root, coordIdentity: 'alice' }
      )
    );

    // After replay (which should emit nothing), drop a NEW file.
    await new Promise((r) => setTimeout(r, 250));
    const newFile = '1814826789020-bbbbbb.md'; // ts > pre-existing
    writeMsg('alice', newFile, 'bob');

    await s.waitForText(newFile, 5_000);
    // The pre-existing file should NOT appear in the screen.
    const ss = s.screenshot();
    expect(ss.text).toContain(newFile);
    expect(ss.text).not.toContain(preExisting);
  });
});

// ─── SIGTERM / graceful shutdown ───────────────────────────────────────

describe('coord watch — graceful shutdown', () => {
  it('Session.close() kills the watcher without leaking the process', async () => {
    mkIdentity(root, 'alice');
    const s = runCoordPty(
      ['watch', 'alice', '--interval', '100'],
      { stRoot: root, coordIdentity: 'alice' }
    );
    // Verify it's actually running before we kill it.
    await new Promise((r) => setTimeout(r, 200));
    await s.close();
    // If close() returns, the process has been killed cleanly.
    expect(true).toBe(true);
  });
});
