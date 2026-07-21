// tests/unit/status-staleness.test.ts — mtime-based staleness fallback.
//
// brief-022: a status file whose mtime is older than STATUS_STALE_MS
// reads as `unknown` regardless of recorded value. The recorded value
// is honored only when the file is fresh enough that we trust the
// owning agent is still alive.

import {
  mkdirSync,
  mkdtempSync,
  rmSync,
  utimesSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import {
  LIVENESS_HEARTBEAT_MS,
  STATUS_LIVENESS_MS,
  STATUS_REFRESH_MS,
  STATUS_STALE_MS,
} from '../../src/common.ts';
import {
  readIdentityLiveness,
  readIdentityStatus,
} from '../../src/commands/status.ts';

let scratch: string;
let stRoot: string;

beforeEach(() => {
  scratch = mkdtempSync(join(tmpdir(), 'st-status-staleness-'));
  stRoot = join(scratch, 'smalltalk');
  mkdirSync(join(stRoot, 'alice'), { recursive: true });
});

afterEach(() => {
  rmSync(scratch, { recursive: true, force: true });
});

function writeStatus(id: string, value: string): string {
  const path = join(stRoot, id, 'status');
  writeFileSync(path, `${value}\n`);
  return path;
}

function backdate(path: string, ageMs: number): void {
  const t = Date.now() - ageMs;
  utimesSync(path, new Date(t), new Date(t));
}

describe('readIdentityStatus — mtime staleness', () => {
  it('fresh file → reads the recorded value', () => {
    writeStatus('alice', 'available');
    expect(readIdentityStatus('alice', stRoot)).toBe('available');
  });

  it('file mtime within the staleness window → recorded value still trusted', () => {
    const path = writeStatus('alice', 'busy');
    backdate(path, STATUS_STALE_MS - 60_000); // 1 min under the threshold
    expect(readIdentityStatus('alice', stRoot)).toBe('busy');
  });

  it('file mtime older than STATUS_STALE_MS → returns `unknown`', () => {
    const path = writeStatus('alice', 'available');
    backdate(path, STATUS_STALE_MS + 60_000); // 1 min past the threshold
    expect(readIdentityStatus('alice', stRoot)).toBe('unknown');
  });

  it('stale-yet-recorded `offline` also surfaces as `unknown`', () => {
    // The point of staleness is "we don't trust what's recorded" — the
    // recorded value doesn't matter, only the mtime.
    const path = writeStatus('alice', 'offline');
    backdate(path, STATUS_STALE_MS + 60_000);
    expect(readIdentityStatus('alice', stRoot)).toBe('unknown');
  });

  it('missing file → `offline` (unchanged from pre-brief-022 behavior)', () => {
    // No status file at all is distinct from a stale one: an agent that
    // never wrote status isn't necessarily dead, it just hasn't booted
    // through the ritual yet. LAYOUT-004 says this case is `offline`.
    expect(readIdentityStatus('alice', stRoot)).toBe('offline');
  });

  it('corrupt contents on a fresh file → `offline` (brief-006 rule)', () => {
    writeStatus('alice', 'garbage-value');
    expect(readIdentityStatus('alice', stRoot)).toBe('offline');
  });

  it('corrupt contents on a stale file → `unknown` (staleness wins)', () => {
    const path = writeStatus('alice', 'garbage-value');
    backdate(path, STATUS_STALE_MS + 60_000);
    // mtime is checked before contents — stale is stale regardless.
    expect(readIdentityStatus('alice', stRoot)).toBe('unknown');
  });
});

// ─── #102: the shared freshness contract ───────────────────────────────
//
// `st agents` said `available` at the same instant `convoy ls --tree`
// said `DEAD (status stale 3m ago)`. The premise in the issue — that
// `st agents` had NO freshness window — turned out to be wrong: it has
// one, STATUS_STALE_MS, but that window answers "do we still trust this
// value?" (15 min, sized for the MCP server's 5-min refresh), not "is
// this agent live?" (~2 min, sized for the ding's 30s heartbeat).
//
// So the divergence was a threshold mismatch, not a missing check.
// `readIdentityLiveness` exposes BOTH windows through one reader so
// consumers stop each inventing their own.
describe('readIdentityLiveness — shared freshness contract', () => {
  it('fresh file → live, recorded value preserved, small age', () => {
    writeStatus('alice', 'available');
    const l = readIdentityLiveness('alice', stRoot);
    expect(l.status).toBe('available');
    expect(l.recorded).toBe('available');
    expect(l.live).toBe(true);
    expect(l.ageMs).toBeLessThan(5_000);
  });

  it('the reported case: 3m old reads NOT live while status stays `available`', () => {
    // This is exactly the disagreement from the issue. The trust window
    // (15 min) is untouched, so `status` is still `available` — but
    // `live` is false, which is what convoy was reporting.
    const path = writeStatus('alice', 'available');
    backdate(path, 3 * 60_000);
    const l = readIdentityLiveness('alice', stRoot);
    expect(l.status).toBe('available'); // trust window: unchanged
    expect(l.live).toBe(false); // liveness window: not demonstrably up
    expect(l.ageMs).toBeGreaterThan(2.5 * 60_000);
  });

  it('inside the liveness window → live', () => {
    const path = writeStatus('alice', 'busy');
    backdate(path, STATUS_LIVENESS_MS - 30_000);
    expect(readIdentityLiveness('alice', stRoot).live).toBe(true);
  });

  it('past the liveness window but inside trust → live=false, status kept', () => {
    const path = writeStatus('alice', 'busy');
    backdate(path, STATUS_LIVENESS_MS + 30_000);
    const l = readIdentityLiveness('alice', stRoot);
    expect(l.live).toBe(false);
    expect(l.status).toBe('busy');
  });

  it('stale-but-was-`busy` stays distinguishable from clean `offline`', () => {
    // The debugging signal the issue asked us not to collapse: an agent
    // that died mid-work is not the same as one that shut down.
    const busyPath = writeStatus('alice', 'busy');
    backdate(busyPath, STATUS_STALE_MS + 60_000);
    mkdirSync(join(stRoot, 'carol'), { recursive: true });
    const offPath = writeStatus('carol', 'offline');
    backdate(offPath, STATUS_STALE_MS + 60_000);

    const wasBusy = readIdentityLiveness('alice', stRoot);
    const wasOffline = readIdentityLiveness('carol', stRoot);

    // Both derive to `unknown` (the trust window is blown) …
    expect(wasBusy.status).toBe('unknown');
    expect(wasOffline.status).toBe('unknown');
    // … but `recorded` keeps them apart.
    expect(wasBusy.recorded).toBe('busy');
    expect(wasOffline.recorded).toBe('offline');
  });

  it('missing file → not live, no age, no recorded value', () => {
    // Distinct from a recorded `offline`: nothing was ever claimed.
    const l = readIdentityLiveness('alice', stRoot);
    expect(l.status).toBe('offline');
    expect(l.live).toBe(false);
    expect(l.ageMs).toBeNull();
    expect(l.recorded).toBeNull();
  });

  it('corrupt contents → recorded is null, status falls back to `offline`', () => {
    writeStatus('alice', 'garbage-value');
    const l = readIdentityLiveness('alice', stRoot);
    expect(l.status).toBe('offline');
    expect(l.recorded).toBeNull();
    expect(l.live).toBe(true); // the FILE is fresh; its contents are not usable
  });

  it('livenessMs is overridable so a consumer can be stricter', () => {
    const path = writeStatus('alice', 'available');
    backdate(path, 30_000);
    expect(readIdentityLiveness('alice', stRoot).live).toBe(true);
    expect(
      readIdentityLiveness('alice', stRoot, { livenessMs: 10_000 }).live
    ).toBe(false);
  });

  it('the two windows are deliberately different (regression guard)', () => {
    // Tightening STATUS_STALE_MS down to the liveness window would flap
    // MCP-refreshed agents (5-min refresh) into `unknown` between every
    // refresh. Keep them apart.
    expect(STATUS_LIVENESS_MS).toBeLessThan(STATUS_STALE_MS);
    expect(STATUS_REFRESH_MS).toBeLessThan(STATUS_STALE_MS);
    // The liveness window must clear several ding heartbeats.
    expect(STATUS_LIVENESS_MS).toBeGreaterThanOrEqual(
      3 * LIVENESS_HEARTBEAT_MS
    );
  });
});
