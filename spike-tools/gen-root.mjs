#!/usr/bin/env node
// gen-root.mjs — synthesize a realistic $COORD_ROOT for the issue-#2 spike.
//
// Layout produced (all knobs via env):
//   IDENTITIES   number of identity folders                  (default 200)
//   ARCHIVE      archive entries per identity                (default 5000)
//   INBOX_IDS    how many identities have live inbox items   (default 5)
//   INBOX_EACH   inbox items per "live" identity             (default 10)
//   TWINS        per identity, archive entries that ALSO have a byte-identical
//                inbox twin (un-swept) so sweep() has real compare+rm work
//                                                              (default 5)
//   CHAIN        length of a cross-identity reply chain for `thread`
//                                                              (default 20)
//   OUT          target root dir                              (required)
//
// Deterministic: no Date.now()/random — filenames are base+counter, rand6 is
// a base32 encoding of a per-identity counter. Re-running with the same knobs
// reproduces the same tree.

import { mkdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

const env = process.env;
const IDENTITIES = Number(env.IDENTITIES ?? 200);
const ARCHIVE = Number(env.ARCHIVE ?? 5000);
const INBOX_IDS = Number(env.INBOX_IDS ?? 5);
const INBOX_EACH = Number(env.INBOX_EACH ?? 10);
const TWINS = Number(env.TWINS ?? 5);
const CHAIN = Number(env.CHAIN ?? 20);
const OUT = env.OUT;
if (!OUT) {
  console.error('OUT=<dir> required');
  process.exit(2);
}

const BASE_TS = 1700000000000;
const B32 = '0123456789abcdefghjkmnpqrstvwxyz';
let tsCounter = 0;
function nextTs() {
  // 13-digit ms, strictly increasing, stays 13 digits for ~300 years of room.
  return BASE_TS + tsCounter++;
}
function rand6(n) {
  let out = '';
  for (let i = 0; i < 6; i++) {
    out += B32[(n >> (5 * i)) & 31];
  }
  return out;
}
let fnCounter = 0;
function genFilename() {
  return `${nextTs()}-${rand6(fnCounter++)}.md`;
}

function msg({ from, subject, inReplyTo, body }) {
  let fm = '---\n';
  fm += `from: ${from}\n`;
  if (subject) fm += `subject: ${JSON.stringify(subject)}\n`;
  if (inReplyTo) fm += `in-reply-to: ${inReplyTo}\n`;
  fm += '---\n';
  return fm + (body ?? 'synthetic body for spike benchmarking.\n');
}

const id = (i) => `id-${String(i).padStart(4, '0')}`;

console.error(
  `gen: ${IDENTITIES} ids x ${ARCHIVE} archive (+${TWINS} twins), ` +
    `${INBOX_IDS}x${INBOX_EACH} inbox, chain=${CHAIN} -> ${OUT}`
);

const t0 = Date.now();
let files = 0;
for (let i = 0; i < IDENTITIES; i++) {
  const name = id(i);
  const inbox = join(OUT, name, 'inbox');
  const archive = join(OUT, name, 'archive');
  mkdirSync(inbox, { recursive: true });
  mkdirSync(archive, { recursive: true });
  // status file so members/overview have something to stat.
  writeFileSync(join(OUT, name, 'status'), 'available\n');

  for (let a = 0; a < ARCHIVE; a++) {
    const fn = genFilename();
    const content = msg({
      from: id((i + 1) % IDENTITIES),
      subject: `archived ${a}`,
    });
    writeFileSync(join(archive, fn), content);
    files++;
    // First TWINS archive entries also get an identical inbox twin so
    // sweep() exercises its readFile-compare + rm path (not just the stat).
    if (a < TWINS) {
      writeFileSync(join(inbox, fn), content);
      files++;
    }
  }
}

// Live inbox items on the first INBOX_IDS identities (no archive twin →
// these survive sweep, represent "current work").
for (let i = 0; i < Math.min(INBOX_IDS, IDENTITIES); i++) {
  const inbox = join(OUT, id(i), 'inbox');
  for (let k = 0; k < INBOX_EACH; k++) {
    const fn = genFilename();
    writeFileSync(
      join(inbox, fn),
      msg({ from: id((i + 7) % IDENTITIES), subject: `live ${k}` })
    );
    files++;
  }
}

// Cross-identity reply chain for `thread`. Alternates between two ids,
// each message in-reply-to the previous, all placed in archive/ (a real
// resolved conversation). Returns the seed (last) filename.
const A = id(0);
const B = id(1);
let prev = undefined;
let seed = undefined;
for (let c = 0; c < CHAIN; c++) {
  const who = c % 2 === 0 ? A : B;
  const recipient = c % 2 === 0 ? B : A;
  const fn = genFilename();
  const content = msg({
    from: who,
    subject: `chain ${c}`,
    inReplyTo: prev,
  });
  // The message lives in the RECIPIENT's archive (that's where a sent +
  // archived message ends up under the tombstone model).
  writeFileSync(join(OUT, recipient, 'archive', fn), content);
  files++;
  prev = fn;
  seed = fn;
}

const secs = ((Date.now() - t0) / 1000).toFixed(1);
console.error(`gen: wrote ${files} files in ${secs}s`);
// Emit the thread seed filename on stdout so the bench harness can use it.
console.log(seed);
