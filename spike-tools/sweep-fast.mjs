#!/usr/bin/env node
// sweep-fast.mjs — PROBE (read-only; does not touch src/). Demonstrates that
// the existsSync-per-archive-entry stat storm in common.ts sweep() can be
// replaced by one readdir(inbox) + set-intersection, with ZERO per-entry
// stats and the identical byte-compare guard. Times both against ROOT.
//
//   ROOT=<dir> node spike-tools/sweep-fast.mjs
//
// Neither variant here removes files (compare-only) so the two are timed on
// the same on-disk state and the run is repeatable.

import { existsSync, readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';

const ROOT = process.env.ROOT;
if (!ROOT) { console.error('ROOT=<dir> required'); process.exit(2); }
const FN = /^[0-9]{13}-[0-9a-z]{6}\.md$/;

// Faithful re-impl of the CURRENT sweep's traversal (compare-only; no rm).
function sweepCurrent(root) {
  let candidates = 0, stats = 0;
  const ids = readdirSync(root);
  for (const id of ids) {
    let arch;
    try { arch = readdirSync(join(root, id, 'archive')); } catch { continue; }
    for (const name of arch) {
      if (!FN.test(name)) continue;
      const inboxPath = join(root, id, 'inbox', name);
      stats++;
      if (!existsSync(inboxPath)) continue;          // <-- per-entry stat storm
      const a = readFileSync(join(root, id, 'archive', name));
      const b = readFileSync(inboxPath);
      if (a.equals(b)) candidates++;
    }
  }
  return { candidates, stats };
}

// Proposed: readdir(inbox) once per identity, set-intersect with archive.
function sweepFast(root) {
  let candidates = 0, stats = 0;
  const ids = readdirSync(root);
  for (const id of ids) {
    let arch;
    try { arch = readdirSync(join(root, id, 'archive')); } catch { continue; }
    let inboxNames;
    try { inboxNames = new Set(readdirSync(join(root, id, 'inbox'))); }
    catch { continue; }                               // no inbox -> nothing to reap
    if (inboxNames.size === 0) continue;              // common case: skip entirely
    for (const name of arch) {
      if (!FN.test(name)) continue;
      if (!inboxNames.has(name)) continue;            // in-memory, no syscall
      const a = readFileSync(join(root, id, 'archive', name));
      const b = readFileSync(join(root, id, 'inbox', name));
      if (a.equals(b)) candidates++;
    }
  }
  return { candidates, stats };
}

// Proposed v2: iterate the INBOX (small/active), not the archive (huge).
// The tombstone invariant only ever removes inbox files, and the
// inbox∩archive intersection is always a subset of inbox — so an archive
// entry with no inbox twin needs no work and never needs to be enumerated.
function sweepInboxDriven(root) {
  let candidates = 0, stats = 0;
  const ids = readdirSync(root);
  for (const id of ids) {
    let inboxNames;
    try { inboxNames = readdirSync(join(root, id, 'inbox')); } catch { continue; }
    for (const name of inboxNames) {
      if (!FN.test(name)) continue;
      const archPath = join(root, id, 'archive', name);
      stats++;
      if (!existsSync(archPath)) continue;            // only stat per INBOX file
      const a = readFileSync(archPath);
      const b = readFileSync(join(root, id, 'inbox', name));
      if (a.equals(b)) candidates++;
    }
  }
  return { candidates, stats };
}

function timed(fn) {
  fn(); // warm
  const s = [];
  for (let i = 0; i < 5; i++) {
    const t = process.hrtime.bigint();
    var r = fn();
    s.push(Number(process.hrtime.bigint() - t) / 1e6);
  }
  s.sort((a, b) => a - b);
  return { med: s[2], r };
}

const cur = timed(() => sweepCurrent(ROOT));
const fast = timed(() => sweepFast(ROOT));
const inb = timed(() => sweepInboxDriven(ROOT));
console.log(`current  (existsSync per archive entry): median ${cur.med.toFixed(1).padStart(8)} ms  ` +
  `(${cur.r.stats} stats, ${cur.r.candidates} twins)`);
console.log(`fast     (readdir archive + intersect):  median ${fast.med.toFixed(1).padStart(8)} ms  ` +
  `(${fast.r.stats} stats, ${fast.r.candidates} twins)`);
console.log(`inbox-driven (iterate inbox, not archive): median ${inb.med.toFixed(1).padStart(8)} ms  ` +
  `(${inb.r.stats} stats, ${inb.r.candidates} twins)`);
console.log(`inbox-driven speedup vs current: ${(cur.med / inb.med).toFixed(0)}x`);
