// examples/pi/smalltalk.ts — pi extension that integrates smalltalk into pi.
//
// Drop into `~/.pi/agent/extensions/smalltalk.ts` (or `.pi/extensions/smalltalk.ts`
// for project-local). Pi auto-discovers and hot-reloads. Reads
// $ST_ROOT and $ST_IDENTITY from env.
//
// Two halves:
//   1. Push: subscribes to session_start, watches smalltalk (per-identity),
//      surfaces every new arrival via ctx.ui.notify + a footer status
//      line, AND injects a (debounced) user message via
//      pi.sendUserMessage so the agent actually sees the event.
//      session_shutdown aborts the watcher.
//   2. Verbs: registers `st_msg_send`, `st_msg_ls`, `st_msg_read`,
//      `st_msg_archive`, `st_msg_thread` via pi.registerTool, mirroring
//      the same five tools the MCP server exposes for Codex / Claude
//      Code. Pi has no native MCP support, so registering tools
//      directly is the parity path.
//
// For a multi-identity dispatcher (one extension watching multiple
// peers' inboxes), swap `smalltalk.watch(identity, ...)` for
// `smalltalk.watch(undefined, { all: true, ... })` — that's the
// cross-tree supervisor mode, which reports activity in every
// peer's tree EXCEPT yours. The default
// (`smalltalk.watch(undefined, ...)`) is "watch your own inbox",
// matching the `st watch` CLI default.
//
// Errors: each tool throws on failure (StError or otherwise); pi
// catches and renders. The push side notifies on errors so the user
// sees the problem without blocking the session.

import type {
  ExtensionAPI,
  ExtensionContext,
  SessionStartEvent,
} from "@mariozechner/pi-coding-agent";
import { Type, type Static } from "typebox";

import {
  asFilename,
  asIdentity,
  createSt,
  type St,
  type Identity,
  type MessageWithLocation,
  type WatchEvent,
} from "@compoundingtech/smalltalk";

interface LsItem {
  filename: string;
  ts: number;
  from: string | null;
  subject: string | null;
  inReplyTo: string | null;
  tags: string[];
  priority: "low" | "normal" | "high" | null;
}

const STATUS_KEY = "smalltalk";

// ─── Lazy st init ──────────────────────────────────────────────────

let cachedSt: { smalltalk: St; identity: Identity } | undefined;
let cachedError: Error | undefined;

function getSt(): { smalltalk: St; identity: Identity } {
  if (cachedSt) return cachedSt;
  if (cachedError) throw cachedError;
  const root = process.env.ST_ROOT;
  const identity = process.env.ST_IDENTITY;
  if (!root || !identity) {
    cachedError = new Error(
      "smalltalk: ST_ROOT and ST_IDENTITY must both be set"
    );
    throw cachedError;
  }
  try {
    const branded = asIdentity(identity);
    cachedSt = {
      smalltalk: createSt({ root, identity: branded }),
      identity: branded,
    };
    return cachedSt;
  } catch (err) {
    cachedError = err instanceof Error ? err : new Error(String(err));
    throw cachedError;
  }
}

// ─── Tool schemas ─────────────────────────────────────────────────────

const PrioritySchema = Type.Union(
  [Type.Literal("low"), Type.Literal("normal"), Type.Literal("high")],
  { description: "Priority hint." }
);

const SendParams = Type.Object({
  to: Type.String({ description: "Recipient identity (LAYOUT-004 grammar)." }),
  body: Type.String({ description: "Message body. Required, non-empty." }),
  from: Type.Optional(
    Type.String({ description: "Sender identity. Defaults to $ST_IDENTITY." })
  ),
  subject: Type.Optional(Type.String({ description: "Subject line." })),
  inReplyTo: Type.Optional(
    Type.String({ description: "Filename of the message being replied to." })
  ),
  tags: Type.Optional(
    Type.Array(Type.String(), { description: "List of tag strings." })
  ),
  priority: Type.Optional(PrioritySchema),
});
type SendParamsT = Static<typeof SendParams>;

const LsParams = Type.Object({
  identity: Type.Optional(
    Type.String({
      description: "Whose folder to list. Defaults to $ST_IDENTITY.",
    })
  ),
  archive: Type.Optional(
    Type.Boolean({ description: "When true, list archive/ instead of inbox/." })
  ),
  since: Type.Optional(
    Type.Number({
      description:
        "Filter to filenames whose <unix-ms> prefix is >= this value.",
    })
  ),
  from: Type.Optional(
    Type.String({ description: "Match files whose `from:` frontmatter equals this." })
  ),
  withMeta: Type.Optional(
    Type.Boolean({
      description:
        "Include parsed frontmatter for each match (slower; off by default).",
    })
  ),
});
type LsParamsT = Static<typeof LsParams>;

const ReadParams = Type.Object({
  filename: Type.String({ description: "Message filename (LAYOUT-004 grammar)." }),
  identity: Type.Optional(
    Type.String({ description: "Whose folder to read from. Defaults to $ST_IDENTITY." })
  ),
  fromArchive: Type.Optional(
    Type.Boolean({
      description:
        "When true, prefer archive/ first (auto-falls back to inbox).",
    })
  ),
});
type ReadParamsT = Static<typeof ReadParams>;

const ArchiveParams = Type.Object({
  filename: Type.String({ description: "Message filename (LAYOUT-004 grammar)." }),
  identity: Type.Optional(
    Type.String({ description: "Whose folder to archive in. Defaults to $ST_IDENTITY." })
  ),
});
type ArchiveParamsT = Static<typeof ArchiveParams>;

const ThreadParams = Type.Object({
  filename: Type.String({ description: "Seed filename (LAYOUT-004 grammar)." }),
  identity: Type.Optional(
    Type.String({ description: "Identity hint. Defaults to $ST_IDENTITY." })
  ),
  tree: Type.Optional(
    Type.Boolean({
      description:
        "When true, return depth-indented hierarchical view (vs flat chronological default).",
    })
  ),
});
type ThreadParamsT = Static<typeof ThreadParams>;

// ─── Tool helpers ─────────────────────────────────────────────────────

function resolveIdentity(explicit: string | undefined): Identity {
  const { identity } = getSt();
  return explicit !== undefined ? asIdentity(explicit) : identity;
}

function flattenMessage(m: MessageWithLocation): {
  filename: string;
  identity: string;
  folder: "inbox" | "archive";
  message: Record<string, unknown>;
} {
  const message: Record<string, unknown> = {
    from: m.message.from,
    body: m.message.body,
  };
  if (m.message.subject !== undefined) message.subject = m.message.subject;
  if (m.message.inReplyTo !== undefined) message.inReplyTo = m.message.inReplyTo;
  if (m.message.tags !== undefined) message.tags = m.message.tags;
  if (m.message.priority !== undefined) message.priority = m.message.priority;
  return {
    filename: m.filename,
    identity: m.identity,
    folder: m.folder,
    message,
  };
}

// ─── Extension factory ────────────────────────────────────────────────

const ARRIVAL_FLUSH_MS = 300;

interface PendingArrival {
  filename: string;
  subject: string;
  identity: Identity;
}

function formatArrivalBatch(batch: PendingArrival[]): string {
  if (batch.length === 1) {
    const ev = batch[0]!;
    return (
      `[smalltalk] new message in ${ev.identity}/inbox: ${ev.filename} ` +
      `— Subject: ${ev.subject}. Use st_msg_read to read it, then ` +
      `st_msg_send with to=<original from>, inReplyTo=${ev.filename} ` +
      `to respond if appropriate.`
    );
  }
  const lines = batch
    .map((ev) => `  - ${ev.filename} — Subject: ${ev.subject}`)
    .join("\n");
  return (
    `[smalltalk] ${batch.length} new messages in ${batch[0]!.identity}/inbox:\n` +
    `${lines}\n` +
    `Use st_msg_ls + st_msg_read to triage; st_msg_send with inReplyTo ` +
    `to respond.`
  );
}

export default function stExtension(pi: ExtensionAPI): void {
  let ac: AbortController | undefined;

  // ─ Push half: watch + notify ─
  pi.on("session_start", async (_event: SessionStartEvent, ctx: ExtensionContext) => {
    let smalltalk: St;
    let identity: Identity;
    try {
      ({ smalltalk, identity } = getSt());
    } catch (err) {
      ctx.ui.notify(
        `smalltalk: ${err instanceof Error ? err.message : String(err)}; extension idle`,
        "warning"
      );
      return;
    }

    // Guard against `session_start` firing twice without an
    // intervening `session_shutdown` (e.g. /reload during boot,
    // error mid-cleanup). Abort the previous controller before
    // replacing it; the previous watch loop sees `signal.aborted`
    // and exits its for-await.
    if (ac !== undefined) {
      ac.abort();
    }
    ac = new AbortController();
    const signal = ac.signal;

    ctx.ui.notify(`smalltalk: watching ${identity}/inbox/`, "info");
    ctx.ui.setStatus(STATUS_KEY, `smalltalk: ${identity}/inbox/`);

    // Debounced arrival pipeline: chokidar can deliver bursts (rsync
    // dropping N files; manual replay). Per-arrival
    // pi.sendUserMessage would queue N user turns; the LLM serializes
    // them and bloats context. Buffer for ARRIVAL_FLUSH_MS, then
    // flush one consolidated message.
    const pending: PendingArrival[] = [];
    let flushTimer: ReturnType<typeof setTimeout> | undefined;
    const scheduleFlush = (): void => {
      if (flushTimer !== undefined) return;
      flushTimer = setTimeout(() => {
        flushTimer = undefined;
        if (pending.length === 0) return;
        const batch = pending.splice(0, pending.length);
        pi.sendUserMessage(formatArrivalBatch(batch));
      }, ARRIVAL_FLUSH_MS);
    };
    signal.addEventListener("abort", () => {
      if (flushTimer !== undefined) {
        clearTimeout(flushTimer);
        flushTimer = undefined;
      }
      pending.length = 0;
    });

    // brief-027: force a model turn on every session_start (fresh
    // sessions and `/reload`/`/resume`-driven rebinds) so the agent
    // runs the smalltalk boot ritual idempotently. Without this, --resume
    // preserves the conversation but no new turn triggers; the agent
    // stays silent with stale status, an undrained inbox, and no
    // journal entry for whatever it's about to do. Positioned after
    // the watcher pipeline is wired and the abort listener is
    // attached so any backlog the agent decides to drain gets routed
    // through the same scheduleFlush path as live arrivals, but
    // BEFORE the for-await IIFE below so the ritual nudge precedes
    // any chatter that lands in the buffer.
    pi.sendUserMessage(
      "Run the smalltalk boot ritual: set status to available, drain inbox, log/update your current task, write a journal entry if mid-task."
    );

    void (async () => {
      try {
        // Per-identity watch — we want events about messages landing
        // in OUR inbox. Default-undefined now does that already
        // (post-brief-017a), but explicit identity keeps the
        // intent obvious to readers and is a no-op functionally.
        for await (const ev of smalltalk.watch(identity, {
          signal,
          intervalMs: 500,
          withSubject: true,
        }) as AsyncIterable<WatchEvent>) {
          if (ev.folder !== "inbox") continue;
          const subject = ev.subject ?? "(no subject)";
          // Human-facing toast — fires per arrival; the debounce
          // applies to LLM-context injection only.
          ctx.ui.notify(
            `smalltalk: new in ${identity}/inbox — ${subject}`,
            "info"
          );
          pending.push({ filename: ev.filename, subject, identity });
          scheduleFlush();
        }
      } catch (err) {
        if (signal.aborted) return;
        ctx.ui.notify(
          `smalltalk: watch error: ${err instanceof Error ? err.message : String(err)}`,
          "error"
        );
      }
    })();
  });

  pi.on("session_shutdown", async (_event, ctx) => {
    if (ac !== undefined) {
      ac.abort();
      ac = undefined;
    }
    ctx.ui.setStatus(STATUS_KEY, undefined);
  });

  // ─ Verb half: register the five smalltalk tools ─

  pi.registerTool({
    name: "st_msg_send",
    label: "smalltalk: send",
    description:
      "Write a new message to <to>/inbox/. The act of writing IS the send; sync moves the file across machines later.",
    parameters: SendParams,
    async execute(_id, params: SendParamsT) {
      const { smalltalk } = getSt();
      const to = asIdentity(params.to);
      const opts: Parameters<St["send"]>[2] = {};
      if (params.from !== undefined) opts.from = asIdentity(params.from);
      if (params.subject !== undefined) opts.subject = params.subject;
      if (params.inReplyTo !== undefined) {
        opts.inReplyTo = asFilename(params.inReplyTo);
      }
      if (params.tags !== undefined) opts.tags = params.tags;
      if (params.priority !== undefined) opts.priority = params.priority;
      const filename = await smalltalk.send(to, params.body, opts);
      return {
        content: [{ type: "text", text: `sent: ${to}/${filename}` }],
        details: { filename, identity: to },
      };
    },
  });

  pi.registerTool({
    name: "st_msg_ls",
    label: "smalltalk: ls",
    description:
      "List filenames in <identity>/inbox/ (or archive/) in chronological order. Identity defaults to $ST_IDENTITY.",
    parameters: LsParams,
    async execute(_id, params: LsParamsT) {
      const { smalltalk } = getSt();
      const id = resolveIdentity(params.identity);
      const opts: Parameters<St["ls"]>[1] = {};
      if (params.archive !== undefined) opts.archive = params.archive;
      if (params.since !== undefined) opts.since = params.since;
      if (params.from !== undefined) opts.fromFilter = asIdentity(params.from);
      const filenames = await smalltalk.ls(id, opts);
      const summary = `${filenames.length} ${filenames.length === 1 ? "match" : "matches"} in ${id}/${params.archive === true ? "archive" : "inbox"}`;
      const details: { matches: string[]; identity: string; items?: LsItem[] } =
        {
          matches: filenames,
          identity: id,
        };
      if (params.withMeta === true) {
        // smalltalk.ls() in withMeta mode returns Filename[]; the items live
        // on the underlying LsResult. The embeddable surface flattens to
        // filenames-only, so re-fetch via the lower-level CLI helper or
        // accept that pi callers get filenames + ts derived per-item.
        // For the tool, we re-read each filename's frontmatter via
        // smalltalk.read to populate items consistently with the MCP path.
        const items: LsItem[] = [];
        for (const fn of filenames) {
          try {
            const r = await smalltalk.read(id, asFilename(fn));
            items.push({
              filename: fn,
              ts: Number(fn.slice(0, 13)),
              from: r.message.from || null,
              subject: r.message.subject ?? null,
              inReplyTo: r.message.inReplyTo ?? null,
              tags: r.message.tags ?? [],
              priority: r.message.priority ?? null,
            });
          } catch {
            items.push({
              filename: fn,
              ts: Number(fn.slice(0, 13)),
              from: null,
              subject: null,
              inReplyTo: null,
              tags: [],
              priority: null,
            });
          }
        }
        details.items = items;
      }
      return {
        content: [{ type: "text", text: summary }],
        details,
      };
    },
  });

  pi.registerTool({
    name: "st_msg_read",
    label: "smalltalk: read",
    description:
      "Return the parsed message at <identity>/inbox/<filename> (or archive/, with auto-fallback). identity defaults to $ST_IDENTITY.",
    parameters: ReadParams,
    async execute(_id, params: ReadParamsT) {
      const { smalltalk } = getSt();
      const id = resolveIdentity(params.identity);
      const filename = asFilename(params.filename);
      const opts: Parameters<St["read"]>[2] = {};
      if (params.fromArchive !== undefined) opts.fromArchive = params.fromArchive;
      const r = await smalltalk.read(id, filename, opts);
      return {
        content: [{ type: "text", text: `${r.folder}/${r.identity}/${r.filename}` }],
        details: flattenMessage(r),
      };
    },
  });

  pi.registerTool({
    name: "st_msg_archive",
    label: "smalltalk: archive",
    description:
      "Move <identity>/inbox/<filename> to <identity>/archive/. Idempotent: a file already in archive returns success unchanged.",
    parameters: ArchiveParams,
    async execute(_id, params: ArchiveParamsT) {
      const { smalltalk } = getSt();
      const id = resolveIdentity(params.identity);
      const filename = asFilename(params.filename);
      await smalltalk.archive(id, filename);
      return {
        content: [{ type: "text", text: `archived: ${id}/${filename}` }],
        details: { filename, identity: id },
      };
    },
  });

  pi.registerTool({
    name: "st_msg_thread",
    label: "smalltalk: thread",
    description:
      "Return every message reachable from <filename> via in-reply-to (both directions, cross-identity). Default = flat chronological; tree=true preserves the depth-indented hierarchy.",
    parameters: ThreadParams,
    async execute(_id, params: ThreadParamsT) {
      const { smalltalk } = getSt();
      const id = resolveIdentity(params.identity);
      const filename = asFilename(params.filename);
      const opts: Parameters<St["thread"]>[2] = {};
      if (params.tree !== undefined) opts.tree = params.tree;
      const messages = await smalltalk.thread(id, filename, opts);
      const summary =
        messages.length === 1
          ? "1 message in thread"
          : `${messages.length} messages in thread`;
      return {
        content: [{ type: "text", text: summary }],
        details: { messages: messages.map(flattenMessage) },
      };
    },
  });
}
