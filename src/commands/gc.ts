// commands/gc.ts — `st gc serve`: the tombstone garbage-collector service.
//
// A dedicated, long-running, per-network service that continuously enforces
// the LAYOUT archive-as-tombstone invariant by running the SAME convergence
// sweep as `st sync sweep`, on an interval. It exists so archive stays durable
// no matter what a syncer does: a union sync (one lacking `--delete`), or a
// mis-rooted one, can re-add an already-archived inbox file, and the gc
// removes it within one cycle — instead of the zombie lingering until someone
// happens to `read` it (the read-path lazy-sweep) or run a manual sweep.
//
// Two host paths, one codebase (Nathan's "self-contained but better together"
// steer):
//   - standalone: `st gc serve` — a smalltalk-only user gets durable archive
//     with no convoy.
//   - convoy-hosted: `convoy up` supervises `st gc serve` as a per-network
//     child, so convoy users run nothing extra. The wiring is convoy's; this
//     stays a plain foreground process with clean start/stop for it to host.

import { rootShapeWarning, sweep } from '../common.ts';
import { invokedName, type CliContext } from '../cli-context.ts';

const DEFAULT_INTERVAL_S = 2;
/** Floor on --interval so a fat-fingered tiny value can't busy-spin the disk. */
const MIN_INTERVAL_MS = 100;

export interface GcServeOptions {
  /** Bus root to sweep. Defaults to $ST_ROOT at the CLI layer. */
  root: string;
  /** Sweep cadence in ms. */
  intervalMs: number;
  /** Run a single sweep and exit (cron / test hook). */
  once: boolean;
}

/**
 * Abortable sleep — resolves immediately when `signal` aborts, so a SIGTERM
 * mid-interval stops the service promptly instead of waiting out the cadence.
 */
function delay(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    if (signal.aborted) {
      resolve();
      return;
    }
    const onAbort = (): void => {
      clearTimeout(timer);
      resolve();
    };
    const timer = setTimeout(() => {
      signal.removeEventListener('abort', onAbort);
      resolve();
    }, ms);
    signal.addEventListener('abort', onAbort, { once: true });
  });
}

/**
 * Run the gc service loop: sweep `root` every `intervalMs` until SIGINT/
 * SIGTERM. Idempotent and cheap in steady state — `sweep` only byte-compares
 * inbox files that ALSO exist in archive (the candidate zombies); a clean bus
 * does a readdir + existsSync and no reads. Emits a startup line and a
 * wrong-root WARN (so a mis-rooted service is loud, not a silent no-op), and a
 * one-line banner only on cycles that actually remove something. `--once` runs
 * a single sweep and returns (cron / test hook).
 */
export async function cmdGcServe(
  opts: GcServeOptions,
  ctx: CliContext
): Promise<number> {
  const warn = rootShapeWarning(opts.root);
  if (warn !== null) ctx.stderr(`${warn}\n`);

  if (opts.once) {
    const r = sweep(opts.root);
    ctx.stderr(`# gc: swept ${r.removed} redundant inbox file(s)\n`);
    return 0;
  }

  const ac = new AbortController();
  const onSig = (): void => ac.abort();
  process.once('SIGINT', onSig);
  process.once('SIGTERM', onSig);

  ctx.stderr(
    `# gc serve: root=${opts.root} interval=${opts.intervalMs}ms (SIGINT/SIGTERM to stop)\n`
  );
  try {
    while (!ac.signal.aborted) {
      const r = sweep(opts.root);
      if (r.removed > 0) {
        ctx.stderr(`# gc: removed ${r.removed} resurrected inbox file(s)\n`);
      }
      await delay(opts.intervalMs, ac.signal);
    }
  } finally {
    process.removeListener('SIGINT', onSig);
    process.removeListener('SIGTERM', onSig);
  }
  ctx.stderr('# gc serve: stopped\n');
  return 0;
}

// ─── CLI ─────────────────────────────────────────────────────────────────

export function cmdGcCli(
  args: readonly string[],
  ctx: CliContext
): Promise<number> | number {
  const name = invokedName(ctx.env);
  const usage =
    `usage: ${name} gc serve [--root PATH] [--interval S] [--once]\n\n` +
    '  serve  run the tombstone garbage-collector: sweep the bus root every\n' +
    '         --interval seconds (default 2) to enforce archive-as-tombstone,\n' +
    '         so a resurrected inbox copy is removed within one cycle. Runs\n' +
    '         until SIGINT/SIGTERM. --once runs a single sweep and exits.\n' +
    '         --root defaults to $ST_ROOT.\n';

  const sub = args[0];
  if (sub === undefined || sub === '-h' || sub === '--help') {
    ctx.stderr(usage);
    return sub === undefined ? 1 : 0;
  }
  if (sub !== 'serve') {
    ctx.stderr(`unknown subcommand: ${sub}\n${usage}`);
    return 1;
  }

  let root = ctx.stRoot;
  let intervalMs = DEFAULT_INTERVAL_S * 1000;
  let once = false;
  const rest = args.slice(1);
  for (let i = 0; i < rest.length; i++) {
    const a = rest[i]!;
    switch (a) {
      case '--root': {
        const v = rest[++i];
        if (v === undefined) throw new Error('--root requires a value');
        root = v;
        break;
      }
      case '--interval': {
        const v = rest[++i];
        if (v === undefined) throw new Error('--interval requires a value');
        const s = Number(v);
        if (!Number.isFinite(s) || s <= 0) {
          throw new Error(
            `--interval must be a positive number of seconds: ${v}`
          );
        }
        intervalMs = Math.max(MIN_INTERVAL_MS, Math.round(s * 1000));
        break;
      }
      case '--once':
        once = true;
        break;
      case '-h':
      case '--help':
        ctx.stderr(usage);
        return 0;
      default:
        throw new Error(`unknown flag: ${a}`);
    }
  }
  return cmdGcServe({ root, intervalMs, once }, ctx);
}
