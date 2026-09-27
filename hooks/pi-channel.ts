// st2 native message delivery for the pi harness.
//
// pi has no MCP and no app-server. Its integration point is an extension loaded into the live
// interactive process, which is strictly better than either: `pi.sendUserMessage()` is a typed
// injection point and `ctx.isIdle()` is a positive idle proof, so st2 never inspects a screen to
// decide whether delivery is safe.
//
// This asset is published as part of st2's immutable content-addressed hook set and referenced from
// a launch as `<verified set>/pi-channel.ts`. It deliberately holds no policy: st2 decides which
// message is delivered, how (`deliverAs` travels on the frame), and what a starting session is told
// about its own durable state (`sessionContext` travels on the hello). This file only moves frames.
//
// It fails open in both directions. An unmanaged pi session — one started by a human, with none of
// the channel environment set — loads this extension and does nothing. A channel that does not
// answer in time starts the session anyway, without restored context, rather than hanging it.
import childProcess from "node:child_process";
import type {
  ExtensionAPI,
  ExtensionContext,
  SessionShutdownEvent,
} from "@earendil-works/pi-coding-agent";

const PROTOCOL = 1;

const BIN = "ST2_PI_CHANNEL_BIN";
const CATALOG = "ST2_PI_CHANNEL_CATALOG";
const IDENTITY = "ST2_PI_CHANNEL_IDENTITY";
const RUNTIME_ID = "ST2_PI_CHANNEL_RUNTIME_ID";
const SESSION = "ST2_PI_CHANNEL_SESSION";
const SEQ = "ST2_PI_CHANNEL_SEQ";

// pi starts the session even if st2 is slow to answer. Restored context is worth a short wait and
// never worth a hung agent.
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
 * Both halves must outlive an extension instance. pi re-instantiates extensions on session
 * replacement, so an instance-local `current` would leave the previous session's channel running
 * beside the new one — measured: `/new` produced two live channels watching one inbox, each with
 * its own delivered set, which is the duplicate-delivery shape this extension exists to avoid.
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
   * frame omitting the cost would erase the published one on the very next turn boundary. Holding
   * the last one here and restating it is what keeps `costUsd` meaning "the last assistant
   * message's cost" instead of flickering to null between messages.
   */
  lastCostUsd?: number;
};

/**
 * Withhold rather than coerce (HC-R03).
 *
 * pi reports `tokens: null` and `percent: null` for real — immediately after a compaction, and
 * across a process restart until the next assistant usage arrives — while `contextWindow` stays
 * populated. That is pi positively saying it does not know, and substituting zero, the previous
 * reading, or a division st2 could have done itself is exactly the fabrication HC-R03 forbids.
 * A non-finite value is treated the same way: `NaN` and `Infinity` are not readings.
 */
const finiteOrNull = (value: unknown): number | null =>
  typeof value === "number" && Number.isFinite(value) ? value : null;

/**
 * Compile-time coupling to pi's own declarations for the three surfaces the context producer reads.
 *
 * The producer reads all three through widened, guarded views, and it has to: an unmanaged session
 * or a build whose telemetry surface moved must still load and still deliver mail, so a missing or
 * throwing pull withholds a number rather than taking a turn down. But a widened cast alone makes
 * that tolerance absolute AND silent — the surface could vanish from pi entirely and every context
 * frame would quietly stop carrying numbers, with nothing in this repository noticing.
 *
 * These declarations are erased at runtime and exist only so the extension type gate fails when
 * pi's shape moves. `ContextUsage`'s own doc comment is the contract HC-R03 is written against:
 * "Estimated context tokens, or null if unknown (e.g. right after compaction, before next LLM
 * response)." What they cannot catch is a change of MEANING with no change of shape — that is what
 * the version-pinned fixtures in `src/pi_channel.rs` are for (HC-R13, HC-T03).
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
 * pi puts its own environment in front of every bash-tool child, so leaving these set would hand a
 * nested pi its parent's bus identity — and, if that nested pi loaded this extension, its parent's
 * inbox. But pi re-instantiates extensions on session replacement, so the values cannot simply be
 * captured in module scope: a second instantiation would find the variables already deleted and
 * silently run as an unmanaged session. Measured — it stopped all delivery after `/new`. The stash
 * outlives re-instantiation; the environment does not.
 */
const stash = (): Stash => {
  const globals = globalThis as { __st2PiChannel?: Stash };
  if (!globals.__st2PiChannel) {
    // EVERY ST2_PI_CHANNEL_* value is stashed and unexported — the ownership pair included: a
    // leaked runtime id or session token would hand a nested pi (or any tool child) this seat's
    // registry key and record ownership. The channel subprocess receives them explicitly below.
    globals.__st2PiChannel = {
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
  return globals.__st2PiChannel;
};

export default function (pi: ExtensionAPI) {
  const state = stash();
  const { bin, catalog, identity, runtimeId, session, seq } = state;

  // Always close a NAMED channel, never "whatever is current". A session replacement (/new,
  // /resume, /fork) tears the old session down around the new one's start, so a teardown handler
  // that closed `current` would reap the successor it just opened — measured, and it silently
  // stopped all delivery after `/new`.
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
    // reaps it. That happens on its own whenever pi exits, however it exits.
    if (!child.stdin?.destroyed) child.stdin?.end();
  };

  /** Open a channel and resolve with the hello's restored context (empty if none, or on timeout). */
  const open = async (ctx: ExtensionContext): Promise<string> => {
    if (!bin || !catalog || !identity) return Promise.resolve("");
    if (typeof ctx.isIdle !== "function") {
      // Refuse rather than degrade. Without a positive idle proof this extension cannot choose
      // between an idle send and a steer, and guessing would deliver into a running turn. No
      // channel means presence decays, so the agent reads as unreachable instead of quietly
      // mis-delivering.
      ctx.ui?.notify?.(
        "st2: this pi build exposes no ctx.isIdle(); refusing to open the st2 channel",
        "error",
      );
      return Promise.resolve("");
    }
    // Closes the channel opened by the PREVIOUS session, whichever extension instance opened
    // it — and WAITS (bounded) for it to exit before the replacement spawns: the successor
    // shares the seat's record, and a predecessor draining its queued frames after the new
    // session's seed would land stale state into fresh records.
    const previous = state.child;
    closeChild(previous);
    await awaitExit(previous, 2000);
    // The predecessor's cost belongs to the predecessor. The stash outlives session replacement
    // by design, so without this a `/new` session's first frames would restate the old session's
    // cost as their own.
    state.lastCostUsd = undefined;

    const channelEnv: NodeJS.ProcessEnv = { ...process.env };
    if (runtimeId) channelEnv[RUNTIME_ID] = runtimeId;
    if (session) channelEnv[SESSION] = session;
    if (seq) channelEnv[SEQ] = seq;
    const child = childProcess.spawn(
      bin,
      ["--catalog", catalog, "driver", "pi-channel", "--identity", identity],
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
      // An observability pipe must never take pi down: a channel that closed its stdin mid-write
      // surfaces EPIPE on the stream, which without a listener is an uncaught exception in the
      // host process. Retire the channel instead — frames simply stop, fail-open.
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
          // A newer control plane may speak a wire this asset was not written against. Refusing is
          // the honest outcome: presence still decays, so the agent reads as unreachable rather
          // than silently never receiving mail.
          if (frame.protocol !== PROTOCOL) {
            closeChild(child);
            ctx.ui?.notify?.(
              `st2: pi channel protocol ${frame.protocol} is not understood by this extension (expected ${PROTOCOL}); reinstall st2's hook set`,
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
          // one is rejected, so the idle proof selects the call shape. It never selects the policy.
          // Not optional-chained: `ctx.isIdle?.() ?? true` would read a missing idle proof as
          // "idle" and silently turn every mid-turn delivery into a plain send. st2 pins the
          // surfaces whose skew is silent, and this is the one such surface in this file.
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

      // Split on LF only. A generic line reader also splits on Unicode separators, which can appear
      // inside a message body and would corrupt the frame.
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

  // Observed harness state, extension side. pi's own turn boundaries are the positive signal,
  // and the idle edge is `agent_settled`, not `agent_end`: measured against the repo's own pi
  // captures, `ctx.isIdle()` is still false through `agent_end`, and a queued follow-up turn
  // starts exactly at that boundary — an `agent_end` emit would blip a spurious idle before it.
  // `agent_settled` is the first point pi is provably idle. The frame is observational — st2
  // decides what becomes of it — and a closed channel drops it silently, matching the fail-open
  // rule this file already follows. pi 0.84.2 exposes no typed waiting-on-a-human event, so no
  // frame here ever claims one.
  const sendFrame = (frame: Record<string, unknown>) => {
    const child = state.child;
    if (!child || !child.stdin || child.stdin.destroyed) return;
    child.stdin.write(JSON.stringify(frame) + "\n");
  };
  const sendState = (word: "active" | "idle") => sendFrame({ type: "state", state: word });
  const boundedTimelineString = (value: unknown, limit = 16_384): string | undefined =>
    typeof value === "string" ? value.slice(0, limit) : undefined;
  const normalizedTimelinePayload = (event: string, raw: unknown): Record<string, unknown> => {
    const value = raw && typeof raw === "object" ? raw as Record<string, unknown> : {};
    if (event === "tool_call") return {
      toolCallId: boundedTimelineString(value.toolCallId ?? value.tool_call_id, 256),
      toolName: boundedTimelineString(value.toolName ?? value.tool_name, 128),
      input: { redacted: true },
    };
    if (event === "tool_result") return {
      toolCallId: boundedTimelineString(value.toolCallId ?? value.tool_call_id, 256),
      isError: value.isError === true,
      content: { redacted: true },
    };
    const rawMessage = value.message;
    const message = rawMessage && typeof rawMessage === "object"
      ? rawMessage as Record<string, unknown>
      : value;
    let content: unknown = boundedTimelineString(message.content);
    if (Array.isArray(message.content)) {
      content = message.content.slice(0, 64).map((part) => {
        if (typeof part === "string") return { text: part.slice(0, 16_384) };
        if (!part || typeof part !== "object") return {};
        return { text: boundedTimelineString((part as Record<string, unknown>).text) };
      });
    }
    const usage = message.usage && typeof message.usage === "object"
      ? message.usage as Record<string, unknown>
      : undefined;
    return { message: {
      id: boundedTimelineString(message.id, 256),
      role: boundedTimelineString(message.role, 32),
      content,
      usage: usage ? {
        input: finiteOrNull(usage.input ?? usage.inputTokens),
        output: finiteOrNull(usage.output ?? usage.outputTokens),
      } : undefined,
    } };
  };
  const sendTimeline = (event: string, payload: unknown) => {
    try {
      sendFrame({ type: "timeline", event, payload: normalizedTimelinePayload(event, payload) });
    } catch {
      // Observability remains fail-open; Rust applies the durable byte/redaction policy.
    }
  };

  // Harness context, extension side (HC-R02, HC-R03, HC-R11, HC-R12).
  //
  // `ctx.getContextUsage()` answers the whole fill triple in one call and rides the ctx of every
  // lifecycle event, so this producer keeps no accumulator and reads no second source. It also
  // holds no cadence policy: st2's harness-context write guard quantizes to 1% of the window, so
  // emitting on every boundary costs at most one write per bucket entered however chatty pi is,
  // and the record's own heartbeat rule decides the rest. Emitting liberally is therefore correct
  // — and the finest boundary is the one that matters. Turn-boundary-only observation was
  // measured at 92% of pre-compaction warnings missed precisely because the wedge case is a
  // single long turn; `message_end` defeats that, because each tool call and its result form
  // their own assistant message on pi.
  //
  // Every pull is guarded and a throw is not a reading: an observability call must never take a
  // turn down, and "we could not read it" is withheld, never zero.
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
      // pi's `tokens` is the last assistant message's `totalTokens` — input + output + cacheRead
      // + cacheWrite. That is NOT what omp's identically-shaped call means (prompt-only input),
      // which is why the two harnesses are separate producers and separate fixtures.
      usedTokens: finiteOrNull(tokens),
      windowTokens: finiteOrNull(contextWindow),
      // Carried raw. pi's percent is a float that runs well above 100 when a turn overruns the
      // window (585.625% measured), and clamping here would hide exactly the saturation this
      // record exists to surface.
      usedPercent: finiteOrNull(percent),
      model: modelId(ctx),
      costUsd: state.lastCostUsd ?? null,
    };
  };

  /**
   * The harness-durable compaction count (HC-R12): pi's own session store answers it, and the
   * answer survives a process restart — measured 2 → 3 across a compaction and read back
   * correctly on the next process's `session_start`. `null` when the store cannot be read, which
   * makes st2 fall back to counting the edge itself; that is a weaker, incarnation-scoped answer
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
   * write. A compaction edge always writes, while a reading whose percent is withheld has no
   * bucket and so lands only on an edge or the heartbeat — so an edge sent alone would publish
   * the STALE pre-compaction numbers beside it, and the null reading proving the window was
   * emptied would not appear until the heartbeat came due. Either half may be absent; a frame
   * with neither is not sent.
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

  // Registered only now that every helper above is initialized. A handler body runs long after
  // module evaluation, so a forward reference would in fact be safe — but a use-before-declaration
  // in this file is precisely the defect class that once shipped green through the type gate, so
  // the registrations stay below the definitions they use.
  pi.on("agent_start", async () => sendState("active"));
  pi.on("agent_settled", async (_event, ctx) => {
    sendState("idle");
    sendContext(ctx);
  });

  const onContextEvent = (name: "message_end" | "turn_end" | "agent_end") =>
    (pi.on as unknown as (
      event: string,
      handler: (event: unknown, ctx: ExtensionContext) => void | Promise<void>,
    ) => void)(name, async (event, ctx) => {
      captureCost(event);
      sendContext(ctx);
      if (name === "message_end") sendTimeline(name, event);
    });
  onContextEvent("message_end");
  onContextEvent("turn_end");
  onContextEvent("agent_end");

  const onTimelineEvent = pi.on.bind(pi) as unknown as (
    event: string,
    handler: (event: unknown) => void | Promise<void>,
  ) => void;
  onTimelineEvent("tool_call", async (event) => sendTimeline("tool_call", event));
  onTimelineEvent("tool_result", async (event) => sendTimeline("tool_result", event));

  // pi's `session_compact` carries `reason ∈ manual | threshold | overflow` — the only v1 producer
  // that names its trigger at all. Measured in the handler itself: `getContextUsage()` already
  // reports `{tokens: null, percent: null}` there, and `getEntries()` already counts the new
  // entry, so this one frame carries both the honest withheld reading and the durable count.
  (pi.on as unknown as (
    event: string,
    handler: (event: unknown, ctx: ExtensionContext) => void | Promise<void>,
  ) => void)("session_compact", async (event, ctx) => {
    const reason = (event as { reason?: unknown })?.reason;
    sendContext(ctx, {
      trigger: typeof reason === "string" ? reason : null,
      count: durableCompactions(ctx),
    });
  });

  pi.on("session_start", async (_event, ctx) => {
    // Awaited before the session's first turn, which is what makes restored context reach the boot
    // prompt rather than the turn after it.
    const restored = await open(ctx);
    const opened = state.child;
    // Do not seed an idle state here. `session_start` precedes a positional boot prompt, and an
    // idle frame would authorize the channel to inject mail before pi has created the transcript
    // for that prompt. `agent_settled` is the first transcript-ready lifecycle edge. Seed only the
    // context record, so a resumed session still publishes the window it resumed INTO
    // rather than waiting for its first turn boundary. A fresh session reads `{tokens: 0}` here
    // and a post-compaction restart reads `{tokens: null}` — both are honest answers pi gives.
    if (opened) sendContext(ctx);
    if (restored.trim()) {
      // A custom message participates in LLM context without triggering a turn of its own — the
      // closest pi equivalent to the other harnesses' `additionalContext` hook output.
      pi.sendMessage(
        { customType: "st2-session-start", content: restored, display: true },
        { deliverAs: "nextTurn" },
      );
    }
    // Bound to the channel this session opened, so a later session's abort cannot reap it.
    ctx.signal?.addEventListener?.("abort", () => closeChild(opened));
  });
  // Only a real quit closes from here. A session replacement also fires `session_shutdown`, and it
  // fires around the successor's `session_start`, so closing on those reasons reaps the channel the
  // new session just opened — measured: delivery stopped entirely after `/new`. Replacement is
  // handled by `open()` closing its predecessor instead.
  pi.on("session_shutdown", async (event: SessionShutdownEvent) => {
    if (event.reason === "quit") closeChild(state.child);
  });
}
