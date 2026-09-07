// st2 native message delivery for the omp harness.
//
// Forked from pi-channel.ts: omp is pi-family and loads the same extension shape, but the two
// diverge where it matters (measured 2026-08-25, omp v18.0.3 — see
// docs/vrs/06-omp-driver/.experiments/). omp has no `agent_settled` event, so terminal
// `agent_end` uses a bounded `ctx.isIdle()` poll; structured `ask` tool events and approval events
// carry the blocked-on-human axis pi cannot express; and a failed turn carries omp's own typed
// error classification, which this asset forwards raw because st2 — not the asset — decides what
// a rejected provider credential is. Like the pi asset this file holds no delivery
// policy: st2 decides which message is delivered, how, and what a starting session is told. It
// fails open in both directions — an unmanaged omp session loads this extension and does nothing;
// a slow channel starts the session
// without restored context rather than hanging it.
import childProcess from "node:child_process";
import type {
  ExtensionAPI,
  ExtensionContext,
} from "@earendil-works/pi-coding-agent";

const PROTOCOL = 1;

const BIN = "ST2_OMP_CHANNEL_BIN";
const CATALOG = "ST2_OMP_CHANNEL_CATALOG";
const IDENTITY = "ST2_OMP_CHANNEL_IDENTITY";
const RUNTIME_ID = "ST2_OMP_CHANNEL_RUNTIME_ID";
const SESSION = "ST2_OMP_CHANNEL_SESSION";
const SEQ = "ST2_OMP_CHANNEL_SEQ";

// omp starts the session even if st2 is slow to answer. Restored context is worth a short wait
// and never worth a hung agent.
const HELLO_TIMEOUT_MS = 5000;

type Frame = {
  type?: string;
  protocol?: number;
  sessionContext?: string;
  content?: string;
  deliverAs?: "steer" | "followUp";
  meta?: Record<string, unknown>;
};

/**
 * Process-wide channel state.
 *
 * Both halves must outlive an extension instance: omp re-instantiates extensions on session
 * replacement, so an instance-local `current` would leave the previous session's channel running
 * beside the new one, each with its own delivered set — the duplicate-delivery shape this
 * extension exists to avoid.
 */
type Stash = {
  bin?: string;
  catalog?: string;
  identity?: string;
  runtimeId?: string;
  session?: string;
  seq?: string;
  child?: childProcess.ChildProcess;
  /**
   * The last assistant message's `usage.cost.total`.
   *
   * Cost rides only the message-bearing events, but a context frame may be emitted from an event
   * that carries none (`agent_end`, `session_start`). st2's record replaces a reading's fields
   * WHOLESALE — deliberately, so a withheld number is never fabricated from a previous one — so a
   * frame omitting the cost would erase the published one on the very next turn boundary.
   */
  lastCostUsd?: number;
  /** The structured `ask` tool call currently waiting for its matching result. */
  pendingAskToolCallId?: string;
  /** Generation fencing every bounded settle poll against newer activity. */
  settleGeneration?: number;
};

/**
 * Withhold rather than coerce (HC-R03). A non-finite or absent value is not a reading, and
 * substituting zero, the previous number, or a division st2 could have done itself is exactly the
 * fabrication HC-R03 forbids.
 */
const finiteOrNull = (value: unknown): number | null =>
  typeof value === "number" && Number.isFinite(value) ? value : null;

const REASON_MAX_CHARS = 240;

/** Keep diagnostic state bounded and stable across multiline/model-authored input. */
const boundedReason = (value: unknown): string | undefined => {
  if (typeof value !== "string") return undefined;
  const normalized = value.trim().replace(/\s+/gu, " ");
  if (!normalized) return undefined;
  const chars = [...normalized];
  return chars.length <= REASON_MAX_CHARS
    ? normalized
    : `${chars.slice(0, REASON_MAX_CHARS - 1).join("")}…`;
};

type AgentEndFrame = {
  willContinue?: unknown;
  messages?: unknown;
};

/**
 * omp's own words for the provider error that ended a turn.
 *
 * `errorId` is omp's error-classification BITFIELD, assigned onto the assistant message at the
 * provider-stream boundary beside `errorStatus` and `errorMessage`. It travels raw: st2 owns the
 * verdict, exactly as it owns delivery policy, so this asset never decides what an auth failure
 * is. `errorStatus` is deliberately not carried — the status code classifies nothing (three of
 * the four measured 403s are not credential rejections) and omp already prefixes it to the prose.
 * Measured on omp 18.1.7; see `docs/vrs/06-omp-driver/.experiments/`.
 */
type ProviderError = {
  reason: string;
  errorId?: number;
};

/** A terminal provider error requires operator action; it is not an idle settle. */
const terminalProviderError = (event: AgentEndFrame): ProviderError | undefined => {
  if (!Array.isArray(event.messages)) return undefined;
  for (let index = event.messages.length - 1; index >= 0; index -= 1) {
    const message = event.messages[index];
    if (!message || typeof message !== "object") continue;
    if (!("role" in message) || message.role !== "assistant") continue;
    if (!("stopReason" in message) || message.stopReason !== "error") return undefined;
    const error: ProviderError = {
      reason: boundedReason("errorMessage" in message ? message.errorMessage : undefined) ??
        "assistant error",
    };
    const classification = finiteOrNull("errorId" in message ? message.errorId : undefined);
    if (classification !== null) error.errorId = classification;
    return error;
  }
  return undefined;
};

/**
 * Compile-time coupling to the pinned pi declarations for the surfaces the context producer reads.
 *
 * Same reasoning as `pi-channel.ts`: the producer reads them through widened, guarded views so a
 * build whose telemetry surface moved still loads and still delivers mail, and a widened cast alone
 * would make that tolerance absolute and silent. Erased at runtime.
 *
 * Note what this can and cannot prove for omp. It pins the SHAPE against pi 0.84.2's typings, which
 * is all this asset compiles against — omp ships no typings of its own. It cannot prove omp's
 * `tokens` still means prompt-only input, because that is a meaning and not a shape; the
 * version-pinned fixture in `src/pi_channel.rs` is what bounds that (HC-R13, HC-T03).
 */
type PinnedContextUsage = NonNullable<ReturnType<ExtensionContext["getContextUsage"]>>;
const pinnedTelemetrySurface: {
  usage: (usage: PinnedContextUsage) => {
    tokens: number | null;
    contextWindow: number;
    percent: number | null;
  };
  modelId: (ctx: ExtensionContext) => string | undefined;
  entries: (ctx: ExtensionContext) => { type: string }[];
} = {
  usage: (usage) => usage,
  modelId: (ctx) => ctx.model?.id,
  entries: (ctx) => ctx.sessionManager.getEntries(),
};
void pinnedTelemetrySurface;

/**
 * Read the channel configuration once and unexport it.
 *
 * Every ST2_OMP_CHANNEL_* value is stashed and unexported: a leaked runtime id or session token
 * would hand a nested harness child this seat's registry key and record ownership. The values
 * cannot live in module scope because a second instantiation would find them already deleted and
 * silently run as an unmanaged session; the stash outlives re-instantiation, the environment does
 * not.
 */
const stash = (): Stash => {
  const globals = globalThis as { __st2OmpChannel?: Stash };
  if (!globals.__st2OmpChannel) {
    globals.__st2OmpChannel = {
      bin: process.env[BIN],
      catalog: process.env[CATALOG],
      identity: process.env[IDENTITY],
      runtimeId: process.env[RUNTIME_ID],
      session: process.env[SESSION],
      seq: process.env[SEQ],
    };
    delete process.env[BIN];
    delete process.env[CATALOG];
    delete process.env[IDENTITY];
    delete process.env[RUNTIME_ID];
    delete process.env[SESSION];
    delete process.env[SEQ];
  }
  return globals.__st2OmpChannel;
};

/**
 * ctx.isIdle() guarded: omp's proof must never take the session down, and a throw reads as "not
 * proven idle", the conservative answer for every caller.
 */
const idleProof = (ctx: ExtensionContext): boolean => {
  try {
    return ctx.isIdle();
  } catch {
    return false;
  }
};

export default function (pi: ExtensionAPI) {
  const state = stash();
  const { bin, catalog, identity, runtimeId, session, seq } = state;


  const cancelSettle = () => {
    state.settleGeneration = (state.settleGeneration ?? 0) + 1;
  };
  // Always close a NAMED channel, never "whatever is current". A session replacement tears the
  // old session down around the new one's start, so a teardown handler that closed `current`
  // would reap the successor it just opened (measured on pi; same lifecycle shape here).
  const awaitExit = (child: childProcess.ChildProcess | undefined, ms: number) =>
    new Promise<void>((resolve) => {
      if (!child || child.exitCode !== null || child.signalCode !== null) return resolve();
      const timer = setTimeout(resolve, ms);
      timer.unref?.();
      child.once("exit", () => {
        clearTimeout(timer);
        resolve();
      });
    });

  const closeChild = (child: childProcess.ChildProcess | undefined) => {
    if (!child) return;
    if (state.child === child) state.child = undefined;
    // The channel treats EOF on its stdin as the session boundary, so ending this pipe is what
    // reaps it. That happens on its own whenever omp exits, however it exits.
    if (!child.stdin?.destroyed) child.stdin?.end();
  };

  /** Open a channel and resolve with the hello's restored context (empty if none, or on timeout). */
  const open = async (ctx: ExtensionContext): Promise<string> => {
    if (!bin || !catalog || !identity) return Promise.resolve("");
    if (typeof ctx.isIdle !== "function") {
      // Refuse rather than degrade. Without a positive idle proof this extension cannot choose
      // between an idle send and a steer, and guessing would deliver into a running turn.
      ctx.ui?.notify?.(
        "st2: this omp build exposes no ctx.isIdle(); refusing to open the st2 channel",
        "error",
      );
      return Promise.resolve("");
    }
    // Close the PREVIOUS session's channel and wait (bounded) before spawning: the successor
    // shares the seat's record, and a predecessor draining its queued frames after the new
    // session's seed would land stale state into fresh records.
    const previous = state.child;
    closeChild(previous);
    await awaitExit(previous, 2000);
    // The predecessor's cost belongs to the predecessor. The stash outlives session replacement
    // by design, so without this a replacement session's first frames would restate the old
    // session's cost as their own.
    state.lastCostUsd = undefined;
    state.pendingAskToolCallId = undefined;

    cancelSettle();
    const channelEnv: NodeJS.ProcessEnv = { ...process.env };
    if (runtimeId) channelEnv[RUNTIME_ID] = runtimeId;
    if (session) channelEnv[SESSION] = session;
    if (seq) channelEnv[SEQ] = seq;
    const child = childProcess.spawn(
      bin,
      ["--catalog", catalog, "driver", "omp-channel", "--identity", identity],
      { stdio: ["pipe", "pipe", "inherit"], env: channelEnv },
    );
    state.child = child;

    return new Promise<string>((resolve) => {
      let settled = false;
      const settle = (value: string) => {
        if (settled) return;
        settled = true;
        resolve(value);
      };
      const timer = setTimeout(() => settle(""), HELLO_TIMEOUT_MS);
      timer.unref?.();
      child.on("error", () => {
        if (state.child === child) state.child = undefined;
        settle("");
      });
      // An observability pipe must never take the host down: EPIPE on a closed stdin is an
      // uncaught exception without a listener. Retire the channel instead — fail-open.
      child.stdin.on("error", () => {
        if (state.child === child) state.child = undefined;
      });
      child.on("exit", () => settle(""));

      const send = (frame: Record<string, unknown>) => {
        if (child.stdin.destroyed) return;
        child.stdin.write(JSON.stringify(frame) + "\n");
      };

      const handle = async (line: string) => {
        let frame: Frame;
        try {
          frame = JSON.parse(line);
        } catch {
          return;
        }
        if (frame.type === "hello") {
          // A newer control plane may speak a wire this asset was not written against. Refusing
          // is the honest outcome: presence still decays, so the agent reads as unreachable
          // rather than silently never receiving mail.
          if (frame.protocol !== PROTOCOL) {
            closeChild(child);
            ctx.ui?.notify?.(
              `st2: omp channel protocol ${frame.protocol} is not understood by this extension (expected ${PROTOCOL}); reinstall st2's hook set`,
            );
            settle("");
            return;
          }
          clearTimeout(timer);
          settle(typeof frame.sessionContext === "string" ? frame.sessionContext : "");
          return;
        }
        if (frame.type !== "message" || typeof frame.content !== "string") return;
        try {
          // `deliverAs` is required only while a turn is streaming, and an idle send that carries
          // one is rejected, so the idle proof selects the call shape. It never selects the
          // policy. Not optional-chained: reading a missing idle proof as "idle" would silently
          // turn every mid-turn delivery into a plain send.
          if (ctx.isIdle()) {
            await pi.sendUserMessage(frame.content);
          } else {
            await pi.sendUserMessage(frame.content, { deliverAs: frame.deliverAs ?? "steer" });
          }
          send({ type: "delivered", meta: frame.meta });
        } catch (error) {
          send({ type: "failed", meta: frame.meta, error: String(error) });
        }
      };

      // Split on LF only. A generic line reader also splits on Unicode separators, which can
      // appear inside a message body and would corrupt the frame.
      let pending = "";
      child.stdout.setEncoding("utf8");
      child.stdout.on("data", (chunk: string) => {
        pending += chunk;
        let index = pending.indexOf("\n");
        while (index >= 0) {
          const line = pending.slice(0, index);
          pending = pending.slice(index + 1);
          if (line.trim()) void handle(line);
          index = pending.indexOf("\n");
        }
      });
    });
  };

  // Frames are observational — st2 decides what becomes of them — and a closed channel drops
  // them silently.
  const sendFrame = (frame: Record<string, unknown>) => {
    const child = state.child;
    if (!child || !child.stdin || child.stdin.destroyed) return;
    child.stdin.write(JSON.stringify(frame) + "\n");
  };

  // The idle edge without `agent_settled`: `ctx.isIdle()` is still false AT `agent_end` and
  // flips true within ~250ms (measured), so idle is the first true sample of a bounded poll
  // after `agent_end`. A queued follow-up turn keeps it false, so no spurious idle blip. A
  // budget exhausted without an idle proof emits nothing: a record nobody can prove ages out
  // rather than restating a stale active.
  const IDLE_POLL_MS = 100;
  const IDLE_POLL_BUDGET_MS = 5000;

  const watchSettle = (ctx: ExtensionContext) => {
    // Starting a newer settle attempt also retires every older one.
    const generation = (state.settleGeneration ?? 0) + 1;
    state.settleGeneration = generation;
    // Bind this poll to the channel that was live when it started. A session
    // replacement inside the polling window would otherwise let a retired
    // context publish `idle` into the SUCCESSOR's channel while it is active.
    const originatingChild = state.child;
    const startedAt = Date.now();
    const poller = setInterval(() => {
      if (
        state.child !== originatingChild ||
        state.settleGeneration !== generation
      ) {
        clearInterval(poller);
        return;
      }
      const idle = idleProof(ctx);
      if (!idle && Date.now() - startedAt < IDLE_POLL_BUDGET_MS) return;
      clearInterval(poller);
      if (idle) sendFrame({ type: "state", state: "idle" });
    }, IDLE_POLL_MS);
    poller.unref?.();
  };

  // Harness context, extension side (HC-R02, HC-R03, HC-R11, HC-R12).
  //
  // The same one call as pi's, and a DIFFERENT meaning. omp's `getContextUsage().tokens` settles
  // to the last assistant message's `input` — prompt tokens alone — where pi's settles to
  // `totalTokens` (input + output + cacheRead + cacheWrite). Measured in a controlled lab whose
  // fake provider reported prompt tokens of 900, 9,900, and 22,500: `tokens` returned exactly
  // those, while the same messages' `totalTokens` were larger. So omp under-reports relative to
  // pi by output plus cache write on an otherwise identical API, and that is why this is a
  // separate producer with a separate fixture rather than a shared one (HC-T03).
  //
  // The call is present and working on 18.0.3 as well as 18.0.9 (35 occurrences in each binary);
  // the older capture's "ctx exposes {ui} only" was a probe artifact, not a version fact.
  //
  // As on pi, this asset holds no cadence policy — st2's write guard quantizes to 1% of the
  // window — and every pull is guarded, because an observability call must never take a turn down.
  const modelId = (ctx: ExtensionContext): string | null => {
    try {
      const id = (ctx as { model?: { id?: unknown } }).model?.id;
      return typeof id === "string" && id !== "" ? id : null;
    } catch {
      return null;
    }
  };

  const usageReading = (ctx: ExtensionContext): Record<string, unknown> | undefined => {
    const read = (ctx as { getContextUsage?: () => unknown }).getContextUsage;
    if (typeof read !== "function") return undefined;
    let usage: unknown;
    try {
      usage = read.call(ctx);
    } catch {
      return undefined;
    }
    if (!usage || typeof usage !== "object") return undefined;
    const { tokens, contextWindow, percent } = usage as {
      tokens?: unknown;
      contextWindow?: unknown;
      percent?: unknown;
    };
    return {
      // Prompt-only input, NOT `totalTokens`. See the comment above; this is one of the two
      // version-coupled constants HC-T03 names.
      usedTokens: finiteOrNull(tokens),
      windowTokens: finiteOrNull(contextWindow),
      // Carried raw: omp's percent is a float that runs above 100 on an overrun (562.5% measured),
      // and clamping here would hide the saturation this record exists to surface.
      usedPercent: finiteOrNull(percent),
      model: modelId(ctx),
      costUsd: state.lastCostUsd ?? null,
    };
  };

  /**
   * The harness-durable compaction count (HC-R12) — omp's own session store, through the same
   * `getEntries()` path as pi's, measured going 0 → 1 across the event. `null` when the store
   * cannot be read, which makes st2 count the edge itself: a weaker, incarnation-scoped answer
   * and never a wrong one.
   */
  const durableCompactions = (ctx: ExtensionContext): number | null => {
    try {
      const entries = (
        ctx as { sessionManager?: { getEntries?: () => unknown } }
      ).sessionManager?.getEntries?.();
      if (!Array.isArray(entries)) return null;
      return entries.filter((entry) => (entry as { type?: unknown })?.type === "compaction").length;
    } catch {
      return null;
    }
  };

  /**
   * One frame carries the reading and the compaction edge together, because they must land in one
   * write: a compaction edge always writes, while a reading whose percent is withheld has no
   * bucket and lands only on an edge or the heartbeat. Either half may be absent; a frame with
   * neither is not sent.
   */
  const sendContext = (ctx: ExtensionContext, compaction?: Record<string, unknown>) => {
    const reading = usageReading(ctx);
    if (!reading && !compaction) return;
    const frame: Record<string, unknown> = { type: "context" };
    if (reading) frame.reading = reading;
    if (compaction) frame.compaction = compaction;
    sendFrame(frame);
  };

  /** Cost rides the message-bearing events only; hold the last one so no frame erases it. */
  const captureCost = (event: unknown) => {
    const total = (event as { message?: { usage?: { cost?: { total?: unknown } } } })?.message
      ?.usage?.cost?.total;
    if (typeof total === "number" && Number.isFinite(total)) state.lastCostUsd = total;
  };

  // Registered only now that every helper above is initialized: a use-before-declaration in this
  // file is the defect class that once shipped green through the type gate.
  pi.on("agent_start", async () => {
    cancelSettle();
    sendFrame({ type: "state", state: "active" });
  });
  pi.on("agent_end", async (event, ctx) => {
    captureCost(event);
    sendContext(ctx);
    const end = event as AgentEndFrame;
    // A retried error does not end a turn: omp fires `agent_end` with `willContinue: true` for
    // every transient failure it is about to try again (measured: a 429 repeated seven times in
    // one print-mode run). Nothing is claimed from those — least of all about a credential.
    if (end.willContinue === true) {
      cancelSettle();
      return;
    }
    // The typed turn result, on every turn that ACTUALLY ended and in both directions. It is one
    // frame rather than two because the credential edge and the categorical state are the same
    // observation seen from two axes, and correlating them across frames would be a race st2
    // cannot win. An ordinary end carries no error and asserts no state: the sampled idle below
    // still owns that edge.
    const error = terminalProviderError(end);
    sendFrame(error ? { type: "turn", error } : { type: "turn" });
    if (error) {
      cancelSettle();
      return;
    }
    watchSettle(ctx);
  });

  const onWidened = pi.on.bind(pi) as unknown as (
    event: string,
    handler: (event: unknown, ctx: ExtensionContext) => void | Promise<void>,
  ) => void;
  // The finest boundary that carries a fresh reading. Turn-boundary-only observation was measured
  // at 92% of pre-compaction warnings missed, because the wedge case is a single long turn.
  for (const name of ["message_end", "turn_end"]) {
    onWidened(name, async (event, ctx) => {
      captureCost(event);
      sendContext(ctx);
    });
  }
  // omp's `session_compact` carries NO `reason` and no `willRetry` — pi 0.84.2 has both — so the
  // trigger is withheld here and st2 records `unknown`. omp does name its auto-compaction "idle"
  // and "threshold" internally, but those words are not projected onto the event, and inventing
  // one would be a claim no capture supports. Unlike pi, omp's `getContextUsage()` still answers
  // inside this handler (8,100 measured, not null), so the frame carries a real post-compaction
  // reading alongside the durable count.
  onWidened("session_compact", async (_event, ctx) => {
    sendContext(ctx, { trigger: null, count: durableCompactions(ctx) });
  });

  // omp's structured ask tool is observable at its real blocking interval: `tool_call` fires
  // before the dialog and the matching `tool_result` only after the operator answers. Keep the
  // correlating id in process-wide state so an unrelated concurrent result cannot clear the ask.
  type ToolCallFrame = {
    toolName?: unknown;
    toolCallId?: unknown;
    input?: unknown;
  };
  type ToolResultFrame = { toolCallId?: unknown };
  const firstAskQuestion = (event: ToolCallFrame): string | undefined => {
    if (
      event.toolName !== "ask" ||
      typeof event.toolCallId !== "string" ||
      event.toolCallId.trim() === ""
    ) {
      return undefined;
    }
    if (!event.input || typeof event.input !== "object" || !("questions" in event.input)) {
      return undefined;
    }
    const questions = event.input.questions;
    if (!Array.isArray(questions)) return undefined;
    for (const question of questions) {
      if (!question || typeof question !== "object" || !("question" in question)) continue;
      const reason = boundedReason(question.question);
      if (reason) return reason;
    }
    return undefined;
  };

  onWidened("tool_call", async (rawEvent) => {
    // Pinned pi declarations do not know OMP's tool events; the handler validates fields below.
    const event = rawEvent as ToolCallFrame;
    const question = firstAskQuestion(event);
    if (!question || typeof event.toolCallId !== "string") return;
    state.pendingAskToolCallId = event.toolCallId;
    cancelSettle();
    sendFrame({
      type: "state",
      state: "active",
      blockedOn: "human",
      ask: "question",
      reason: question,
    });
  });
  onWidened("tool_result", async (rawEvent, ctx) => {
    const event = rawEvent as ToolResultFrame;
    if (
      typeof event.toolCallId !== "string" ||
      event.toolCallId !== state.pendingAskToolCallId
    ) {
      return;
    }
    state.pendingAskToolCallId = undefined;
    if (idleProof(ctx)) {
      sendFrame({ type: "state", state: "idle" });
      return;
    }
    sendFrame({ type: "state", state: "active" });
    watchSettle(ctx);
  });

  // Approval events are an independent human-blocking surface. omp's pinned pi typings do not
  // declare them, so register through the same widened `on` view.
  type ApprovalFrame = { toolName?: unknown };
  onWidened("tool_approval_requested", async (rawEvent) => {
    if (state.pendingAskToolCallId) return;
    const event = rawEvent as ApprovalFrame;
    const tool = typeof event.toolName === "string" ? event.toolName : "unknown";
    cancelSettle();
    sendFrame({
      type: "state",
      state: "active",
      blockedOn: "human",
      ask: "permission",
      reason: tool,
    });
  });
  onWidened("tool_approval_resolved", async (_event, ctx) => {
    if (state.pendingAskToolCallId) return;
    if (idleProof(ctx)) {
      sendFrame({ type: "state", state: "idle" });
      return;
    }
    sendFrame({ type: "state", state: "active" });
    watchSettle(ctx);
  });

  onWidened("session_before_compact", async () => {
    // Rust owns the durable context path and the write-if-blank policy. The extension carries only
    // the observed edge, so it cannot accidentally overwrite authored state itself.
    sendFrame({ type: "pre_compact" });
  });

  pi.on("session_start", async (_event, ctx) => {
    // Awaited before the session's first turn, which is what makes restored context reach the boot
    // prompt rather than the turn after it.
    const restored = await open(ctx);
    const opened = state.child;
    // Seed the observed state with the idle proof's answer at open time, so the record does not
    // wait for the first turn boundary to exist.
    if (opened) {
      sendFrame({
        type: "state",
        state: idleProof(ctx) ? "idle" : "active",
      });
      // And seed the context record, so a resumed session publishes the window it resumed INTO
      // rather than waiting for its first turn boundary.
      sendContext(ctx);
    }
    if (restored.trim()) {
      // A custom message participates in LLM context without triggering a turn of its own.
      pi.sendMessage(
        { customType: "st2-session-start", content: restored, display: true },
        { deliverAs: "nextTurn" },
      );
    }
  });
  // Upstream's `session_shutdown` payload has no reason: source defines it as process exit only.
  // Replacement is a separate session-switch lifecycle and remains handled by `open()` closing
  // the named predecessor.
  pi.on("session_shutdown", async () => {
    cancelSettle();
    state.pendingAskToolCallId = undefined;
    closeChild(state.child);
  });
}
