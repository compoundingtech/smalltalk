// tests/integration/watch-listener-leak.test.ts — regression guard for a
// CPU leak in the watch poll loop.
//
// `sleepWithSignal` used to add an `abort` listener to the caller's
// AbortSignal on every poll but only remove it on abort (`{ once: true }`
// self-removes only when the event fires, not on the normal timeout
// path). A long-running `st ding` polls every 500ms for days, so the
// shared signal accumulated hundreds of thousands of listeners; since
// addEventListener is O(n) in existing listeners, each poll got more
// expensive and the process burned CPU that grew with uptime (~20% on
// 3-10 day-old dings on the Mac). The fix removes the listener on the
// timeout path. This test asserts the listener count stays bounded
// across many poll iterations.

import { mkdirSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { getEventListeners } from 'node:events';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { createSt } from '../../src/lib.ts';
import { asIdentity } from '../../src/types.ts';

let root: string;

beforeEach(() => {
  root = mkdtempSync(join(tmpdir(), 'watch-leak-'));
});
afterEach(() => {
  rmSync(root, { recursive: true, force: true });
});

describe('watch poll loop — abort-listener leak regression', () => {
  it('does not accumulate abort listeners on the signal across many polls', async () => {
    const identity = asIdentity('probe.agent');
    mkdirSync(join(root, identity, 'inbox'), { recursive: true });
    mkdirSync(join(root, identity, 'archive'), { recursive: true });

    const st = createSt({ root, identity });
    const ac = new AbortController();

    const loop = (async () => {
      for await (const _ev of st.watch(identity, {
        sinceNow: true,
        intervalMs: 2,
        signal: ac.signal,
      })) {
        /* idle inbox — no events */
      }
    })();

    // ~150 poll iterations at 2ms over 300ms. Pre-fix each leaks one
    // listener; fixed keeps at most the single in-flight sleep's listener.
    await new Promise((r) => setTimeout(r, 300));
    const listeners = getEventListeners(ac.signal, 'abort').length;

    ac.abort();
    await loop;

    expect(listeners).toBeLessThan(5);
  }, 10_000);
});
