#!/usr/bin/env node
// bench.mjs — warm-cache timing of the four hot commands against a synthetic
// $COORD_ROOT. Imports the typed cores directly (no per-run node startup) so
// the numbers reflect FS work, not process spawn.
//
//   ROOT=<dir> SEED=<thread-seed.md> IDENTITY=id-0002 ITERS=7 \
//     node --experimental-strip-types spike-tools/bench.mjs
//
// Reports min / median ms over ITERS warm iterations for each op.

import { sweep } from '../src/common.ts';
import { cmdLs } from '../src/commands/ls.ts';
import { cmdSend } from '../src/commands/send.ts';
import { cmdThread } from '../src/commands/thread.ts';
import { cmdOverview } from '../src/commands/overview.ts';

const ROOT = process.env.ROOT;
const SEED = process.env.SEED;
const IDENTITY = process.env.IDENTITY ?? 'id-0002';
const ITERS = Number(process.env.ITERS ?? 7);
if (!ROOT) {
  console.error('ROOT=<dir> required');
  process.exit(2);
}
const env = { COORD_IDENTITY: IDENTITY };

function time1(fn) {
  const t = process.hrtime.bigint();
  fn();
  return Number(process.hrtime.bigint() - t) / 1e6; // ms
}
function bench(label, fn, iters = ITERS) {
  // one warm-up (not counted) then `iters` measured
  try {
    fn();
  } catch (e) {
    console.log(`${label.padEnd(28)}  ERROR: ${e.message}`);
    return;
  }
  const samples = [];
  for (let i = 0; i < iters; i++) samples.push(time1(fn));
  samples.sort((a, b) => a - b);
  const min = samples[0];
  const med = samples[Math.floor(samples.length / 2)];
  console.log(
    `${label.padEnd(28)}  min ${min.toFixed(1).padStart(9)} ms   ` +
      `median ${med.toFixed(1).padStart(9)} ms`
  );
}

console.log(`# warm bench  root=${ROOT}  identity=${IDENTITY}  iters=${ITERS}`);

// 1. sweep() in isolation — the per-command tax.
bench('sweep()', () => sweep(ROOT));

// 2. ls own inbox (plain, filename-only — the cheap path) WITHOUT presweep.
bench('ls (core, no sweep)', () =>
  cmdLs({ recipient: IDENTITY, env, coordRoot: ROOT })
);

// 3. ls --json (withMeta → reads every matched inbox file).
bench('ls --json (core, no sweep)', () =>
  cmdLs({ recipient: IDENTITY, withMeta: true, env, coordRoot: ROOT })
);

// 4. send (core only — writes one file) WITHOUT presweep.
let n = 0;
bench('send (core, no sweep)', () =>
  cmdSend({
    to: IDENTITY,
    from: 'id-0001',
    subject: `bench ${n++}`,
    body: 'x',
    env,
    coordRoot: ROOT,
  })
);

// 5. thread on the deep cross-identity chain. Capped iters: at full scale
// this re-reads the entire tree once per chain node (O(depth x tree)).
if (SEED) {
  bench('thread (core, no sweep)', () =>
    cmdThread({ filename: SEED, env, coordRoot: ROOT }), Math.min(ITERS, 3)
  );
} else {
  console.log('thread                       SKIPPED (no SEED)');
}

// 6. overview (members --enrich + recent-activity over whole tree).
bench('overview (core, no sweep)', () =>
  cmdOverview({ env, coordRoot: ROOT }), Math.min(ITERS, 3)
);

// Composite: what a real `coord message ls` actually costs = sweep + ls.
bench('ls + presweep (real CLI cost)', () => {
  sweep(ROOT);
  cmdLs({ recipient: IDENTITY, env, coordRoot: ROOT });
});
