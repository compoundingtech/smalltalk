// lib.ts — embeddable createCoord({ root, identity, configRoot? }) factory.
//
// What CLI consumers get from `bin/coord` and what library consumers get
// from `createCoord(...)` are functionally identical — the same code paths
// in src/commands/ underpin both. Library consumers get:
//
// - A typed `Coord` handle with `{ root, identity, configRoot }` baked in.
//   No env-var juggling per call.
// - Promise-returning methods. Async-iterable `watch` for streaming.
// - AbortSignal support on `watch` for cancellation.
// - Branded `Identity` / `Filename` parameters that compile-error if you
//   pass an unvalidated string.
// - Typed errors (CoordError + subclasses) you can pattern-match on with
//   instanceof or a stable `code` string.
// - Zero stdout/stderr writes. The library returns values; the caller
//   decides how to display them.
//
// Sync methods run the universal sweep first (LAYOUT-defined
// archive-as-tombstone invariant — without it, byte-identical inbox
// twins would propagate across machines on push/pull). Non-sync
// methods do NOT presweep — sweep is a convergence operation now, not
// transactional. The existing common.ts `sweep()` is
// what does the work.

import { homedir } from 'node:os';
import { join } from 'node:path';

import { parseFrontmatter, sweep as runSweep } from './common.ts';
import { existsSync, readdirSync, readFileSync } from 'node:fs';

import {
  cmdArchive,
  cmdArchiveTrim,
} from './commands/archive.ts';
import { runDing, type DingDeps } from './commands/ding.ts';
import { cmdLs } from './commands/ls.ts';
import { filenameTimestamp } from './common.ts';
import {
  getAgents,
  type AgentSummary,
  type AgentSummaryEnriched,
} from './commands/agents.ts';
import {
  getOverview,
  type Overview,
} from './commands/overview.ts';
import {
  cmdContextAppend,
  cmdContextRead,
  cmdContextWrite,
} from './commands/context.ts';
import { cmdRead } from './commands/read.ts';
import {
  cmdResourceAdd,
  cmdResourceRead,
  cmdResourceRemove,
  listResourceRecords,
  type ResourceRecord,
} from './commands/resource.ts';
import { cmdSend } from './commands/send.ts';
import { cmdStatus } from './commands/status.ts';
import {
  cmdSyncAll,
  cmdSyncAllPull,
  cmdSyncAllPush,
  cmdSyncPull,
  cmdSyncPush,
  type RsyncResult,
  type SyncContext,
  type SyncDeps,
} from './commands/sync.ts';
import {
  resolveWatchSetup,
  watchPoll,
  watchReplay,
  type WatchInput,
} from './commands/watch.ts';

import {
  archiveDir as archiveDirCommon,
  ensureIdentityDirs,
  inboxDir as inboxDirCommon,
  validFilename,
  validIdentity,
} from './common.ts';
import { InvalidIdentityError } from './errors.ts';
import { spawnSync } from 'node:child_process';

import {
  asDeliverableFilename,
  asFilename,
  asIdentity,
  type Filename,
  type Identity,
  type MessageWithLocation,
  type Peer,
  type Priority,
  type Resource,
  type ResourceWithLocation,
  type State,
  type WatchEvent,
} from './types.ts';

// ─── Public option types ────────────────────────────────────────────────

export interface CoordOptions {
  /** $ST_ROOT — parent of every `<identity>/` folder. */
  root: string;
  /** Default identity for any method that doesn't take one explicitly. */
  identity: Identity;
  /** $ST_CONFIG — defaults to `~/.config/smalltalk`. */
  configRoot?: string;
}

export interface SendOptions {
  /** Sender identity. Defaults to the Coord's `identity`. */
  from?: Identity;
  subject?: string;
  inReplyTo?: Filename;
  tags?: string[];
  priority?: Priority;
}

export interface LsOptions {
  archive?: boolean;
  /** Only files whose <unix-ms> prefix is >= this value. */
  since?: number;
  /** Only files whose frontmatter `from:` equals this. */
  fromFilter?: Identity;
}

export interface ReadOptions {
  /** When true, prefer archive/ first; falls back to inbox. */
  fromArchive?: boolean;
}

export interface TrimOptions {
  /** Duration spec like `30d`, `12h`, `2w`. */
  olderThan?: string;
  /** Keep this many most-recent files; trim the rest. */
  keepLast?: number;
  /** When true, return the victim list without deleting. */
  dryRun?: boolean;
  /**
   * Issue #8: when true, delete prefix-sibling attachments alongside
   * the canonical `.md` victims. Default false preserves the
   * LAYOUT-004 "coord owns only the .md" semantic.
   */
  withAttachments?: boolean;
  /** Override now() for deterministic --older-than testing. */
  now?: () => number;
}

export interface ArchiveOptions {
  /**
   * Issue #8: when true, also move prefix-sibling attachments — every
   * file in inbox/ whose `<unix-ms>-<rand6>` prefix matches the
   * canonical `.md`. Default false preserves the LAYOUT-004 "coord
   * owns only the .md" semantic.
   */
  withAttachments?: boolean;
}

/**
 * One orphan attachment entry as returned by {@link Coord.lsOrphans}.
 * `ts` is parsed from the LAYOUT prefix `<unix-ms>`; orphans have no
 * frontmatter to project.
 */
export interface OrphanItem {
  filename: Filename;
  ts: number;
}

export interface ThreadOptions {
  /** Currently a stub (the underlying walk is global either way). */
  tree?: boolean;
}

export interface WatchOptions {
  withSubject?: boolean;
  since?: number;
  sinceNow?: boolean;
  /** Polling interval in ms (default 500). */
  intervalMs?: number;
  /** AbortSignal to cancel the iterable. */
  signal?: AbortSignal;
  /**
   * When true, watch every peer's inbox EXCEPT the Coord's own
   * identity (cross-tree supervisor mode). Default is to watch
   * the Coord's own identity inbox — same default as `st watch`
   * post-brief-017a. Mutually exclusive with the `identity` arg.
   */
  all?: boolean;
}

export interface SyncResult {
  stdout: string;
  stderr: string;
}

export interface FanOutItem {
  peer: Peer;
  ok: boolean;
  stderr?: string;
}

export interface FanOutBidiItem {
  peer: Peer;
  pushOk: boolean;
  pullOk: boolean;
}

// ─── Coord interface ────────────────────────────────────────────────────

export interface Coord {
  readonly root: string;
  readonly identity: Identity;
  readonly configRoot: string;

  send(to: Identity, body: string | Buffer, opts?: SendOptions): Promise<Filename>;
  ls(identity?: Identity, opts?: LsOptions): Promise<Filename[]>;
  read(
    identity: Identity,
    filename: Filename,
    opts?: ReadOptions
  ): Promise<MessageWithLocation>;
  archive(
    identity: Identity,
    filename: Filename,
    opts?: ArchiveOptions
  ): Promise<void>;
  archiveTrim(identity: Identity, opts: TrimOptions): Promise<Filename[]>;
  /**
   * Issue #8: list prefix-sibling attachments in `<identity>/inbox/`
   * (or `<identity>/archive/` with opts.archive) whose canonical `.md`
   * is no longer present in the same folder — i.e. orphans left behind
   * when a `.md` was archived without `--with-attachments`. Returns
   * `{filename, ts}` because orphans have no frontmatter to project.
   * Identity defaults to the Coord's own.
   */
  lsOrphans(
    identity?: Identity,
    opts?: { archive?: boolean }
  ): Promise<OrphanItem[]>;
  thread(
    identity: Identity,
    filename: Filename,
    opts?: ThreadOptions
  ): Promise<MessageWithLocation[]>;
  watch(identity?: Identity, opts?: WatchOptions): AsyncIterable<WatchEvent>;
  getStatus(identity: Identity): Promise<State>;
  setStatus(identity: Identity, state: State): Promise<void>;
  /**
   * Enumerate identities under $ST_ROOT. Mirrors `st agents` but
   * returns the typed payload directly so an embedder can render
   * their own UI without re-parsing.
   */
  agents(opts?: {
    status?: State;
    enrich?: boolean;
  }): AgentSummary[] | AgentSummaryEnriched[];
  /** @deprecated Use {@link Coord.agents}. */
  members(opts?: {
    status?: State;
    enrich?: boolean;
  }): AgentSummary[] | AgentSummaryEnriched[];
  /**
   * brief-028: at-a-glance dashboard for `identity` (defaults to the
   * handle's own identity). Mirrors `st overview`.
   */
  overview(opts?: { identity?: Identity; recent?: number }): Overview;
  /**
   * brief-028: idempotently mkdir `<root>/<name>/{inbox,archive}` so
   * a new identity can host messages. Validates `name` via
   * validIdentity — throws InvalidIdentityError for invalid or
   * reserved names. Returns `{ created: true }` when at least one of
   * the required folders was missing before the call, `{ created:
   * false }` when both already existed.
   */
  createIdentity(name: string): Promise<{ created: boolean }>;
  sweep(): Promise<{ removed: number }>;
  /**
   * brief-009 item 5: read/write annotated URLs an identity surfaces to
   * peers. Lives at `<root>/<identity>/resources/<filename>.md`. Add /
   * remove operate on the handle's OWN identity (single-writer per the
   * LAYOUT encapsulation rule); list / read accept any identity.
   */
  resources: {
    add(input: {
      url: string;
      title?: string;
      tags?: string[];
      /** Optional free-form relation: `owns` / `relates-to` /
       *  `depends-on` are canonical (non-enforced); agents may invent
       *  their own. Never inferred — absent by default. */
      relation?: string;
      body?: string;
    }): Promise<Filename>;
    list(identity?: Identity): Promise<ResourceWithLocation[]>;
    read(identity: Identity, filename: Filename): Promise<Resource>;
    remove(filename: Filename): Promise<void>;
  };
  /**
   * brief-024 (context/ v1): per-agent durable working-state, the
   * in-context-state leg of lossless-restart. Two files live under
   * `<root>/<identity>/context/`:
   *   - now.md: whole-file rewrite; last-write-wins snapshot of
   *     what an agent is mid-doing.
   *   - decisions.md: append-only log of decisions + why.
   * All three verbs are absent-able: reads on a missing folder return
   * empty + `absent: true`; writes lazy-create the folder. The eval's
   * control arm can just `rm -rf context/` to A/B against the
   * treatment.
   */
  context: {
    read(input?: {
      identity?: Identity;
      file?: 'now' | 'decisions' | 'full';
    }): { identity: Identity; file: 'now' | 'decisions' | 'full'; text: string; absent: boolean };
    write(input: {
      body: string;
      identity?: Identity;
    }): { identity: Identity; path: string; bytes: number };
    append(input: {
      decision: string;
      why: string;
      /** Optional caller-supplied ISO timestamp. When omitted, the
       *  handle stamps `new Date().toISOString()`. */
      timestamp?: string;
      identity?: Identity;
    }): {
      identity: Identity;
      /** Absolute path of the entry file that was written. */
      path: string;
      /** Basename of {@link path}. */
      filename: Filename;
      /** The bulleted line stored in the file. */
      line: string;
    };
  };
  sync: {
    push(peer: Peer): Promise<SyncResult>;
    pull(peer: Peer): Promise<SyncResult>;
    pushAll(): Promise<FanOutItem[]>;
    pullAll(): Promise<FanOutItem[]>;
    all(): Promise<FanOutBidiItem[]>;
  };
  /**
   * brief-031 / brief-009 item 4: long-running pty-side notifier — wraps
   * `runDing` for embedders that want to start a ding from inside a TUI
   * or supervisor process instead of shelling out. Resolves when the
   * daemon exits (via `deps.signal` or session-watch). `deps.identity`
   * defaults to the Coord's own identity if you don't override it.
   */
  ding(deps: Omit<DingDeps, 'coord' | 'identity'> & {
    identity?: Identity;
  }): Promise<void>;
}

// ─── Factory ────────────────────────────────────────────────────────────

const DEFAULT_INTERVAL_MS = 500;

export function createCoord(options: CoordOptions): Coord {
  const root = options.root;
  const identity = asIdentity(options.identity);
  const configRoot = options.configRoot ?? join(homedir(), '.config', 'coord');

  const lib_env = {} as NodeJS.ProcessEnv; // empty: API never reads env

  /** Run sweep before sync push/pull so byte-identical inbox/archive
   *  twins don't propagate across machines. NOT called from non-sync
   *  methods — sweep is a convergence operation, not transactional;
   *  see LAYOUT.md "Convergence: archive is a tombstone". */
  function presweep(): void {
    try {
      runSweep(root);
    } catch {
      // best-effort
    }
  }

  // Capturing rsync wrapper for the sync.* family. Both stdout and stderr
  // are captured into the SyncResult/FanOut* return values; nothing leaks
  // to process stdio.
  const capturedRunRsync = (args: string[]): RsyncResult => {
    const r = spawnSync('rsync', ['-a', ...args], {
      stdio: ['inherit', 'pipe', 'pipe'],
      encoding: 'utf8',
    });
    const result: RsyncResult = { status: r.status ?? -1 };
    if (typeof r.stderr === 'string' && r.stderr.length > 0) {
      result.stderr = r.stderr;
    }
    return result;
  };

  function makeSyncCtx(captured: { stdout: string; stderr: string }): SyncContext {
    const deps: SyncDeps = {
      runRsync: capturedRunRsync,
      bannerSink: (line) => {
        captured.stderr += `${line}\n`;
      },
    };
    return { stRoot: root, stConfig: configRoot, deps };
  }

  const coord: Coord = {
    root,
    identity,
    configRoot,

    async send(to, body, opts = {}): Promise<Filename> {
      const r = cmdSend({
        to,
        from: opts.from ?? identity,
        ...(opts.subject !== undefined && { subject: opts.subject }),
        ...(opts.inReplyTo !== undefined && { inReplyTo: opts.inReplyTo }),
        ...(opts.tags !== undefined && { tags: opts.tags }),
        ...(opts.priority !== undefined && { priority: opts.priority }),
        body,
        env: lib_env,
        stRoot: root,
      });
      return asFilename(r.filename);
    },

    async ls(id?, opts = {}): Promise<Filename[]> {
      const r = cmdLs({
        recipient: id ?? identity,
        ...(opts.archive !== undefined && { archive: opts.archive }),
        ...(opts.since !== undefined && { since: opts.since }),
        ...(opts.fromFilter !== undefined && { fromFilter: opts.fromFilter }),
        env: lib_env,
        stRoot: root,
      });
      // Task #128: cmdLs may return outside .md filenames alongside
      // canonical ones. Use the broader validator so `asDeliverableFilename`
      // doesn't throw on the outside path.
      return r.matches.map(asDeliverableFilename);
    },

    async read(id, filename, opts = {}): Promise<MessageWithLocation> {
      const r = cmdRead({
        recipient: id,
        filename,
        ...(opts.fromArchive !== undefined && { fromArchive: opts.fromArchive }),
        env: lib_env,
        stRoot: root,
      });
      // Off-format `.md`: skip the frontmatter reinterpretation. We
      // can't trust `from` (no verified sender), the whole file is
      // body, and `in-reply-to`, `tags`, `priority` would be forged
      // if we projected them. `from: 'outside'` is the marker every
      // outside-path surface uses.
      if (!validFilename(filename)) {
        return {
          message: {
            from: 'outside' as Identity,
            body: readFileSync(r.path, 'utf8'),
          },
          identity: id,
          filename,
          folder: r.label,
        };
      }
      // Build a typed MessageWithLocation from cmdRead's result. cmdRead
      // returns `{ body, header, label, path, untyped }` in formatted mode;
      // we re-parse the file off disk to get the structured frontmatter
      // (cmdRead's header is for human display).
      const text = readFileSync(r.path, 'utf8');
      const parsed = parseFrontmatter(text);
      return toMessageWithLocation(
        parsed.fm,
        parsed.body,
        id,
        filename,
        r.label
      );
    },

    async archive(id, filename, opts = {}): Promise<void> {
      cmdArchive({
        recipient: id,
        filename,
        ...(opts.withAttachments !== undefined && {
          withAttachments: opts.withAttachments,
        }),
        env: lib_env,
        stRoot: root,
      });
    },

    async archiveTrim(id, opts): Promise<Filename[]> {
      const r = cmdArchiveTrim({
        recipient: id,
        ...(opts.olderThan !== undefined && { olderThan: opts.olderThan }),
        ...(opts.keepLast !== undefined && { keepLast: opts.keepLast }),
        ...(opts.dryRun !== undefined && { dryRun: opts.dryRun }),
        ...(opts.withAttachments !== undefined && {
          withAttachments: opts.withAttachments,
        }),
        ...(opts.now !== undefined && { now: opts.now }),
        env: lib_env,
        stRoot: root,
      });
      return r.victims.map(asFilename);
    },

    async lsOrphans(id, opts = {}): Promise<OrphanItem[]> {
      const target = id ?? identity;
      const r = cmdLs({
        recipient: target,
        ...(opts.archive !== undefined && { archive: opts.archive }),
        orphans: true,
        env: lib_env,
        stRoot: root,
      });
      // Orphans have no frontmatter; the LAYOUT-004 timestamp prefix
      // may parse for grammar-conforming siblings but not for arbitrary
      // ones (e.g. `.DS_Store`). Best-effort: try filenameTimestamp,
      // fall back to leading-digit scan, fall back to 0.
      return r.matches.map((fn) => {
        let ts = 0;
        try {
          ts = filenameTimestamp(fn);
        } catch {
          const m = /^(\d{13})/.exec(fn);
          if (m) ts = Number(m[1]);
        }
        return { filename: fn as Filename, ts };
      });
    },

    async thread(id, filename, opts = {}): Promise<MessageWithLocation[]> {
      // The dynamic import keeps the Promise<Module> circle-safe when
      // bundlers tree-shake; static import works the same at runtime.
      const { cmdThread } = await import('./commands/thread.ts');
      const r = cmdThread({
        recipient: id,
        filename,
        ...(opts.tree !== undefined && { tree: opts.tree }),
        env: lib_env,
        stRoot: root,
      });
      // For each line, locate the file, parse it, and build a
      // MessageWithLocation. We re-locate-anywhere because thread's lines
      // don't carry the path/folder back.
      const out: MessageWithLocation[] = [];
      for (const line of r.lines) {
        const located = locateAcrossIdentities(root, line.filename);
        if (located === undefined) {
          // Phantom (extremely unlikely after the cmdThread walk); skip.
          continue;
        }
        const text = readFileSync(located.path, 'utf8');
        const parsed = parseFrontmatter(text);
        out.push(
          toMessageWithLocation(
            parsed.fm,
            parsed.body,
            asIdentity(located.identity),
            asFilename(line.filename),
            located.folder
          )
        );
      }
      return out;
    },

    watch(id?, opts = {}): AsyncIterable<WatchEvent> {
      const watchInput: WatchInput = {
        ...(id !== undefined && { recipient: id }),
        ...(opts.all === true && { all: true }),
        // The library's "who am I" comes from the Coord's identity.
        // Pass it as fromExplicit so the resolver knows which folder
        // is "self" — used both by the new default (singleId = self)
        // and by the --all path's suppression target.
        fromExplicit: identity,
        ...(opts.withSubject !== undefined && { withSubject: opts.withSubject }),
        ...(opts.since !== undefined && { since: opts.since }),
        ...(opts.sinceNow !== undefined && { sinceNow: opts.sinceNow }),
        intervalMs: opts.intervalMs ?? DEFAULT_INTERVAL_MS,
        env: lib_env,
        stRoot: root,
      };
      return watchAsyncIterable(watchInput, opts.signal);
    },

    async getStatus(id): Promise<State> {
      const r = cmdStatus({
        recipient: id,
        env: lib_env,
        stRoot: root,
      });
      if (r.mode !== 'get') throw new Error('unreachable: getStatus mode');
      return r.state;
    },

    async setStatus(id, state): Promise<void> {
      cmdStatus({
        recipient: id,
        setState: state,
        env: lib_env,
        stRoot: root,
      });
    },

    agents(opts = {}): AgentSummary[] | AgentSummaryEnriched[] {
      return getAgents(root, {
        ...(opts.status !== undefined && { status: opts.status }),
        ...(opts.enrich !== undefined && { enrich: opts.enrich }),
      });
    },
    members(opts = {}): AgentSummary[] | AgentSummaryEnriched[] {
      return getAgents(root, {
        ...(opts.status !== undefined && { status: opts.status }),
        ...(opts.enrich !== undefined && { enrich: opts.enrich }),
      });
    },

    overview(opts = {}): Overview {
      const target = opts.identity ?? identity;
      return getOverview(root, target, {
        ...(opts.recent !== undefined && { recent: opts.recent }),
      });
    },

    async createIdentity(name): Promise<{ created: boolean }> {
      if (!validIdentity(name)) {
        throw new InvalidIdentityError(name);
      }
      const alreadyExisted =
        existsSync(inboxDirCommon(name, root)) &&
        existsSync(archiveDirCommon(name, root));
      if (alreadyExisted) return { created: false };
      ensureIdentityDirs(name, root);
      return { created: true };
    },

    async sweep(): Promise<{ removed: number }> {
      return runSweep(root);
    },

    resources: {
      async add(input): Promise<Filename> {
        const r = cmdResourceAdd({
          url: input.url,
          ...(input.title !== undefined && { title: input.title }),
          ...(input.tags !== undefined && { tags: input.tags }),
          ...(input.relation !== undefined && { relation: input.relation }),
          ...(input.body !== undefined && { body: input.body }),
          identity,
          env: lib_env,
          stRoot: root,
        });
        return asFilename(r.filename);
      },
      async list(id?): Promise<ResourceWithLocation[]> {
        const target = id ?? identity;
        const recs = listResourceRecords(target, root);
        return recs.map((rec) => recordToWithLocation(rec, target));
      },
      async read(id, filename): Promise<Resource> {
        const r = cmdResourceRead({
          identity: id,
          filename,
          env: lib_env,
          stRoot: root,
        });
        return recordToResource(r.record);
      },
      async remove(filename): Promise<void> {
        cmdResourceRemove({
          identity,
          filename,
          env: lib_env,
          stRoot: root,
        });
      },
    },

    context: {
      read(input): {
        identity: Identity;
        file: 'now' | 'decisions' | 'full';
        text: string;
        absent: boolean;
      } {
        const target = input?.identity ?? identity;
        const r = cmdContextRead({
          recipient: target,
          file: input?.file ?? 'now',
          env: lib_env,
          stRoot: root,
        });
        return { ...r, identity: asIdentity(r.identity) };
      },
      write(input): {
        identity: Identity;
        path: string;
        bytes: number;
      } {
        const target = input.identity ?? identity;
        const r = cmdContextWrite({
          recipient: target,
          body: input.body,
          env: lib_env,
          stRoot: root,
        });
        return { ...r, identity: asIdentity(r.identity) };
      },
      append(input): {
        identity: Identity;
        path: string;
        filename: Filename;
        line: string;
      } {
        const target = input.identity ?? identity;
        const r = cmdContextAppend({
          recipient: target,
          decision: input.decision,
          why: input.why,
          timestamp: input.timestamp ?? new Date().toISOString(),
          env: lib_env,
          stRoot: root,
        });
        return {
          ...r,
          identity: asIdentity(r.identity),
          filename: asFilename(r.filename),
        };
      },
    },

    async ding(deps): Promise<void> {
      const dingDeps: DingDeps = {
        ...deps,
        coord,
        identity: deps.identity ?? identity,
      };
      return runDing(dingDeps);
    },

    sync: {
      async push(peer): Promise<SyncResult> {
        presweep();
        const captured = { stdout: '', stderr: '' };
        cmdSyncPush(peer, makeSyncCtx(captured));
        return captured;
      },
      async pull(peer): Promise<SyncResult> {
        presweep();
        const captured = { stdout: '', stderr: '' };
        cmdSyncPull(peer, makeSyncCtx(captured));
        return captured;
      },
      async pushAll(): Promise<FanOutItem[]> {
        presweep();
        const captured = { stdout: '', stderr: '' };
        const r = cmdSyncAllPush(makeSyncCtx(captured));
        return [
          ...r.successes.map((p) => ({ peer: p, ok: true })),
          ...r.failures.map((f) => ({
            peer: f.peer,
            ok: false,
            stderr: f.error,
          })),
        ];
      },
      async pullAll(): Promise<FanOutItem[]> {
        presweep();
        const captured = { stdout: '', stderr: '' };
        const r = cmdSyncAllPull(makeSyncCtx(captured));
        return [
          ...r.successes.map((p) => ({ peer: p, ok: true })),
          ...r.failures.map((f) => ({
            peer: f.peer,
            ok: false,
            stderr: f.error,
          })),
        ];
      },
      async all(): Promise<FanOutBidiItem[]> {
        presweep();
        const captured = { stdout: '', stderr: '' };
        const r = cmdSyncAll(makeSyncCtx(captured));
        const items: FanOutBidiItem[] = r.successes.map((p) => ({
          peer: p,
          pushOk: true,
          pullOk: true,
        }));
        // Failures from cmdSyncAll get duplicated push/pull entries; fold
        // them: any peer that doesn't appear in `successes` had at least
        // one direction fail.
        const successSet = new Set(r.successes);
        const failedPeers = new Set(
          r.failures.map((f) => f.peer).filter((p) => !successSet.has(p))
        );
        for (const peer of failedPeers) {
          items.push({ peer, pushOk: false, pullOk: false });
        }
        return items;
      },
    },
  };

  return coord;
}

// ─── Helpers ────────────────────────────────────────────────────────────

function recordToResource(rec: ResourceRecord): Resource {
  const r: Resource = { url: rec.url, body: rec.body };
  if (rec.title !== null) r.title = rec.title;
  if (rec.tags.length > 0) r.tags = rec.tags;
  if (rec.relation !== null) r.relation = rec.relation;
  return r;
}

function recordToWithLocation(
  rec: ResourceRecord,
  identity: string
): ResourceWithLocation {
  return {
    resource: recordToResource(rec),
    identity: asIdentity(identity),
    filename: asFilename(rec.filename),
  };
}

function toMessageWithLocation(
  fm: Record<string, unknown>,
  body: string,
  identity: Identity,
  filename: Filename,
  folder: 'inbox' | 'archive'
): MessageWithLocation {
  // Read-side permissive (LAYOUT-004): missing/malformed frontmatter is an
  // untyped message — body is still readable, `from` is empty rather
  // than throwing on validation. Branding is bypassed for the empty
  // case so untyped files don't break the read API.
  const fromRaw = typeof fm.from === 'string' ? fm.from : '';
  const message: MessageWithLocation['message'] = {
    from: (fromRaw === '' ? '' : asIdentity(fromRaw)) as Identity,
    body,
  };
  if (typeof fm.subject === 'string' && fm.subject.length > 0) {
    message.subject = fm.subject;
  }
  if (typeof fm['in-reply-to'] === 'string' && fm['in-reply-to'].length > 0) {
    message.inReplyTo = asFilename(fm['in-reply-to']);
  }
  if (typeof fm.tags === 'string' && fm.tags.length > 0) {
    // Stored shape is the raw `[a, b]` list scalar; embedders generally
    // want a parsed array. Best-effort split: strip brackets, split on
    // commas, trim and unquote each.
    message.tags = parseTagsScalar(fm.tags);
  } else if (Array.isArray(fm.tags)) {
    message.tags = fm.tags.map((t) => String(t));
  }
  if (
    typeof fm.priority === 'string' &&
    (fm.priority === 'low' || fm.priority === 'normal' || fm.priority === 'high')
  ) {
    message.priority = fm.priority;
  }
  return { message, identity, filename, folder };
}

function parseTagsScalar(s: string): string[] {
  const trimmed = s.replace(/^\[/, '').replace(/\]$/, '');
  return trimmed
    .split(',')
    .map((t) => t.trim().replace(/^"(.*)"$/, '$1').replace(/^'(.*)'$/, '$1'))
    .filter((t) => t.length > 0);
}

/** Locate a filename across every identity's inbox/archive under root. */
function locateAcrossIdentities(
  root: string,
  filename: string
):
  | { path: string; identity: string; folder: 'inbox' | 'archive' }
  | undefined {
  let topEntries: string[];
  try {
    topEntries = readdirSync(root);
  } catch {
    return undefined;
  }
  for (const id of topEntries) {
    for (const folder of ['inbox', 'archive'] as const) {
      const candidate =
        folder === 'inbox'
          ? join(inboxDirCommon(id, root), filename)
          : join(archiveDirCommon(id, root), filename);
      if (existsSync(candidate)) {
        return { path: candidate, identity: id, folder };
      }
    }
  }
  return undefined;
}

// ─── Watch async-iterable adapter ───────────────────────────────────────

async function* watchAsyncIterable(
  input: WatchInput,
  signal: AbortSignal | undefined
): AsyncIterable<WatchEvent> {
  // Validate flags + resolve mode up front. Throws on bad input.
  const { setup, intervalMs } = resolveWatchSetup(input);

  if (signal?.aborted) return;

  // Phase 1: replay.
  const { lines, seen } = watchReplay(setup);
  for (const line of lines) {
    if (signal?.aborted) return;
    yield toWatchEvent(line);
  }

  // Phase 2: poll loop.
  while (!signal?.aborted) {
    await sleepWithSignal(intervalMs, signal);
    if (signal?.aborted) return;
    const polled = watchPoll(setup, seen);
    for (const line of polled.lines) {
      if (signal?.aborted) return;
      yield toWatchEvent(line);
    }
  }
}

function toWatchEvent(line: {
  filename: string;
  identity: string;
  subject?: string;
}): WatchEvent {
  const ev: WatchEvent = {
    filename: asFilename(line.filename),
    identity: asIdentity(line.identity),
    folder: 'inbox',
  };
  if (line.subject !== undefined) ev.subject = line.subject;
  return ev;
}

function sleepWithSignal(
  ms: number,
  signal: AbortSignal | undefined
): Promise<void> {
  return new Promise<void>((resolve) => {
    if (signal?.aborted) {
      resolve();
      return;
    }
    const t = setTimeout(resolve, ms);
    if (signal !== undefined) {
      const onAbort = (): void => {
        clearTimeout(t);
        resolve();
      };
      signal.addEventListener('abort', onAbort, { once: true });
    }
  });
}
