// st native message delivery for the omp harness.
//
// Forked from pi-channel.ts: omp is pi-family and loads the same extension shape, but the two
// diverge where it matters (measured 2026-08-25, omp v18.0.3 — see
// docs/vrs/06-omp-driver/.experiments/). omp has no `agent_settled` event, so terminal
// `agent_end` polls `ctx.isIdle()` until proven idle; structured `ask` tool events and approval events
// carry the blocked-on-human axis pi cannot express; and a failed turn carries omp's own typed
// error classification, which this asset forwards raw because st — not the asset — decides what
// a rejected provider credential is. Like the pi asset this file holds no delivery
// policy: st decides which message is delivered, how, and what a starting session is told. The
// one omp-specific choice here is timing: a message that arrives during a running turn is handed
// to omp at the tool-batch boundary where omp would inject it anyway (see `HOLD_MAX_MS`). It
// fails open in both directions — an unmanaged omp session loads this extension and does nothing;
// a slow channel starts the session
// without restored context rather than hanging it.
import childProcess from "node:child_process";
import type {
  ExtensionAPI,
  ExtensionContext,
} from "@earendil-works/pi-coding-agent";

const PROTOCOL = 2;

const BIN = "ST_OMP_CHANNEL_BIN";
const IDENTITY = "ST_OMP_CHANNEL_IDENTITY";
const RUNTIME_ID = "ST_OMP_CHANNEL_RUNTIME_ID";
const SESSION = "ST_OMP_CHANNEL_SESSION";
const SEQ = "ST_OMP_CHANNEL_SEQ";
const EXPECTED_NATIVE_SESSION = "ST_OMP_CHANNEL_EXPECTED_NATIVE_SESSION";
const RESUME_GENERATION = "ST_OMP_CHANNEL_RESUME_GENERATION";

// omp starts the session even if st is slow to answer. Restored context is worth a short wait
// and never worth a hung agent.
const HELLO_TIMEOUT_MS = 5000;

// omp injects a steer only at a tool-batch boundary or where the run would stop, but a steer that
// is already queued when a batch starts makes every shell and eval call in that batch background
// itself at once ("Backgrounded early to handle an incoming message"). In 23 cross-harness runs on
// 2026-09-26/27, all 176 backgrounded calls started after their steer was queued, while the model
// was still streaming; none was running when the steer arrived. One seat then repeated a send whose
// result it never saw. So a message that arrives during a running turn is held and steered when
// the batch's last `tool_call` returns its `tool_result` (omp announces every call of a batch
// before any runs, and awaits the handler), or when the run ends. Either way it lands where the
// steer would have. A message never waits longer than this behind a running tool call: a longer
// command is backgrounded as before. It was chosen below st3's 15-second work-wake retry.
const HOLD_MAX_MS = 10_000;

type Frame = {
  type?: string;
  protocol?: number;
  sessionContext?: string;
  content?: string;
  deliverAs?: "steer" | "followUp";
  meta?: Record<string, unknown>;
  seat?: { subject?: string; desired?: { display_name?: string }; member?: { display_name?: string; tags?: Record<string, string> } };
};

/** A message the channel sent that omp has not been handed yet. */
type HeldMessage = {
  content: string;
  deliverAs: "steer" | "followUp";
  meta?: Record<string, unknown>;
  /** The channel that sent it. A retired channel's successor re-sends what it never acknowledged. */
  channel: childProcess.ChildProcess;
  ctx: ExtensionContext;
  reply: (frame: Record<string, unknown>) => void;
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
  identity?: string;
  runtimeId?: string;
  session?: string;
  seq?: string;
  expectedNativeSession?: string;
  resumeGeneration?: string;
  child?: childProcess.ChildProcess;
  reconnectTimer?: ReturnType<typeof setTimeout>;
  reconnectAttempt?: number;
  shuttingDown?: boolean;
  /**
   * The last assistant message's `usage.cost.total`.
   *
   * Cost rides only the message-bearing events, but a context frame may be emitted from an event
   * that carries none (`agent_end`, `session_start`). st's record replaces a reading's fields
   * WHOLESALE — deliberately, so a withheld number is never fabricated from a previous one — so a
   * frame omitting the cost would erase the published one on the very next turn boundary.
   */
  lastCostUsd?: number;
  label?: string;
  accepted?: Map<string, { content: string; meta?: Record<string, unknown>; read?: boolean }>;
  /** Todo observation state is fenced by the channel binding, not the extension instance. */
  todoFingerprint?: string;
  todoBranchKey?: string;
  todoLeafKey?: string;
  todoNextPollAt?: number;
  todoSession?: string;
  todoReady?: boolean;

  /** The structured `ask` tool call currently waiting for its matching result. */
  pendingAskToolCallId?: string;
  /** An approval modal currently waiting for the operator. */
  pendingApproval?: boolean;
  /** Generation fencing every settle poll against newer activity. */
  settleGeneration?: number;
  /** Between `agent_start` and the `agent_end` that does not continue. */
  running?: boolean;
  /** Tool calls announced by `tool_call` and not yet answered by `tool_result`. */
  toolCallsInFlight?: Set<string>;
  /** Messages held during a running turn, oldest first. */
  held?: HeldMessage[];
  /** Releases held messages after HOLD_MAX_MS behind a running tool call. */
  holdTimer?: ReturnType<typeof setTimeout>;
  /** Serializes handoffs so omp receives messages in arrival order. */
  handoff?: Promise<void>;
};

/**
 * Withhold rather than coerce (HC-R03). A non-finite or absent value is not a reading, and
 * substituting zero, the previous number, or a division st could have done itself is exactly the
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
 * provider-stream boundary beside `errorStatus` and `errorMessage`. It travels raw: st owns the
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

type TodoStatus = "pending" | "in_progress" | "completed" | "blocked";
type TodoTask = { content: string; status: TodoStatus; blocker?: string };
type TodoPhase = { name: string; tasks: TodoTask[] };
type TodoSnapshot = {
  phases: TodoPhase[];
  totals: Record<TodoStatus | "abandoned", number>;
  truncated: boolean;
};
const record = (value: unknown): Record<string, unknown> | undefined =>
  value !== null && typeof value === "object" && !Array.isArray(value)
    ? value as Record<string, unknown> : undefined;
const todoStatus = (value: unknown): value is TodoStatus =>
  value === "pending" || value === "in_progress" || value === "completed" || value === "blocked";
const utf8Prefix = (value: string, limit: number): string => {
  let bytes = 0;
  let prefix = "";
  for (const character of value) {
    bytes += Buffer.byteLength(character, "utf8");
    if (bytes > limit) break;
    prefix += character;
  }
  return prefix;
};

/** Count the full native snapshot before applying the claim's source-order bounds. */
const boundedTodo = (raw: unknown): TodoSnapshot | undefined => {
  if (!Array.isArray(raw)) return undefined;
  const snapshot: TodoSnapshot = {
    phases: [], totals: { pending: 0, in_progress: 0, completed: 0, blocked: 0, abandoned: 0 }, truncated: false,
  };
  let taskCount = 0;
  for (const rawPhase of raw) {
    const phase = record(rawPhase);
    if (!phase || typeof phase.name !== "string" || !Array.isArray(phase.tasks)) return undefined;
    const name = utf8Prefix(phase.name, 128);
    const output: TodoPhase = { name, tasks: [] };
    const includePhase = snapshot.phases.length < 16;
    if (includePhase) snapshot.phases.push(output);
    if (!includePhase || name !== phase.name) snapshot.truncated = true;
    for (const rawTask of phase.tasks) {
      const task = record(rawTask);
      if (!task || typeof task.content !== "string" ||
        (!todoStatus(task.status) && task.status !== "abandoned") ||
        (task.blocker !== undefined && typeof task.blocker !== "string")) return undefined;
      // OMP's dropped tasks have no corresponding approved claim status. Omit, never relabel.
      if (task.status === "abandoned") { snapshot.totals.abandoned++; continue; }
      if (!todoStatus(task.status)) return undefined;
      snapshot.totals[task.status]++;
      if (!includePhase || taskCount >= 100) { snapshot.truncated = true; continue; }
      const content = utf8Prefix(task.content, 512);
      const blocker = typeof task.blocker === "string" ? utf8Prefix(task.blocker, 512) : undefined;
      if (content !== task.content || blocker !== task.blocker) snapshot.truncated = true;
      output.tasks.push(blocker === undefined ? { content, status: task.status } : { content, status: task.status, blocker });
      taskCount++;
    }
  }
  // Reserve 4 KiB for authenticated session/incarnation provenance added by the Rust producer.
  // JSON escaping can expand each byte sixfold; drop only trailing items, never alter totals.
  let serializedBytes = Buffer.byteLength(JSON.stringify(snapshot), "utf8");
  if (serializedBytes > 60 * 1024 && !snapshot.truncated) {
    snapshot.truncated = true;
    serializedBytes--; // `true` is one byte shorter than `false`.
  }
  while (serializedBytes > 60 * 1024) {
    const last = snapshot.phases.at(-1);
    if (!last) break;
    const removed = last.tasks.length > 0 ? last.tasks.pop() : snapshot.phases.pop();
    const siblings = last.tasks.length > 0 || (removed === last && snapshot.phases.length > 0);
    serializedBytes -= Buffer.byteLength(JSON.stringify(removed), "utf8") + (siblings ? 1 : 0);
  }
  return snapshot;
};

type BranchTodo = { key: string; observedAt: string; sourceOp: string; snapshot: TodoSnapshot };
const branchTodo = (ctx: ExtensionContext): BranchTodo | null | undefined => {
  try {
    // Native OMP's read-only session manager exposes getBranch(), root-to-current-leaf.
    // Never use getEntries(): that includes unrelated branches.
    const entries = ctx.sessionManager.getBranch();
    for (let i = entries.length - 1; i >= 0; i--) {
      const entry = record(entries[i]);
      if (!entry) continue;
      let details: Record<string, unknown> | undefined;
      let sourceOp: string;
      if (entry.type === "custom" && entry.customType === "user_todo_edit") {
        details = record(entry.data);
        sourceOp = "user_edit";
      } else {
        const message = record(entry.message);
        if (entry.type !== "message" || message?.role !== "toolResult" ||
          message.toolName !== "todo" || message.isError === true) continue;
        details = record(message.details);
        if (details?.op === "view" || typeof details?.op !== "string") continue;
        sourceOp = details.op;
      }
      const snapshot = boundedTodo(details?.phases);
      if (!snapshot || typeof entry.timestamp !== "string" || !Number.isFinite(Date.parse(entry.timestamp))) continue;
      return {
        key: `${entry.id}:${entry.timestamp}`, observedAt: new Date(entry.timestamp).toISOString(), sourceOp, snapshot,
      };
    }
    return null; // Readable branch with no snapshot is known empty.
  } catch {
    return undefined; // An unreadable branch is not evidence of an empty list.
  }
};

/**
 * Read the channel configuration once and unexport it.
 *
 * Every ST_OMP_CHANNEL_* value is stashed and unexported: a leaked runtime id or session token
 * would hand a nested harness child this seat's registry key and record ownership. The values
 * cannot live in module scope because a second instantiation would find them already deleted and
 * silently run as an unmanaged session; the stash outlives re-instantiation, the environment does
 * not.
 */
const stash = (): Stash => {
  const globals = globalThis as { __stOmpChannel?: Stash };
  if (!globals.__stOmpChannel) {
    globals.__stOmpChannel = {
      bin: process.env[BIN],
      identity: process.env[IDENTITY],
      runtimeId: process.env[RUNTIME_ID],
      session: process.env[SESSION],
      seq: process.env[SEQ],
      expectedNativeSession: process.env[EXPECTED_NATIVE_SESSION],
      resumeGeneration: process.env[RESUME_GENERATION],
    };
    delete process.env[BIN];
    delete process.env[IDENTITY];
    delete process.env[RUNTIME_ID];
    delete process.env[SESSION];
    delete process.env[SEQ];
    delete process.env[EXPECTED_NATIVE_SESSION];
    delete process.env[RESUME_GENERATION];
  }
  return globals.__stOmpChannel;
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

// Managed seats show `label[short]`. The launcher exports the persona short code; the
// declared name may carry terminal control characters, which never reach the title.
const seatLabel = (label: string): string => {
  const short = process.env.AGENT_PERSONA_SHORT?.trim();
  return (short ? `${label}[${short}]` : label).replace(/\p{Cc}/gu, "");
};

export default function (pi: ExtensionAPI) {
  const state = stash();
  let jobContext: ExtensionContext | undefined;
  let lastStateFrame: Record<string, unknown> | undefined;
  let lastBackgroundJobs: number | null | undefined;
  const backgroundJobs = (): number | null => {
    try {
      const snapshot = (jobContext as ExtensionContext & {
        getAsyncJobSnapshot?: () => { running?: unknown } | null;
      } | undefined)?.getAsyncJobSnapshot?.();
      return Array.isArray(snapshot?.running) ? snapshot.running.length : null;
    } catch { return null; }
  };
  const applyLabel = async (ctx: ExtensionContext) => {
    if (!state.label) return;
    try { await pi.setSessionName(state.label); }
    catch { ctx.ui?.notify?.("st: could not update the session name", "warning"); }
  };
  const { bin, identity, runtimeId, session, seq } = state;
  let expectedNativeSession = state.expectedNativeSession;
  let resumeGeneration = state.resumeGeneration;

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
  const open = async (ctx: ExtensionContext, reconnecting = false): Promise<string> => {
    if (!bin || !identity) return Promise.resolve("");
    if (typeof ctx.isIdle !== "function") {
      // Refuse rather than degrade. Without a positive idle proof this extension cannot choose
      // between an idle send and a steer, and guessing would deliver into a running turn.
      ctx.ui?.notify?.(
        "st: this omp build exposes no ctx.isIdle(); refusing to open the st channel",
        "error",
      );
      return Promise.resolve("");
    }
    const nativeSessionId = ctx.sessionManager.getSessionId();
    if (typeof nativeSessionId !== "string" || nativeSessionId.trim() === "") {
      ctx.ui?.notify?.(
        "st: omp reported no native session id; refusing to open the st channel",
        "error",
      );
      return Promise.resolve("");
    }
    state.shuttingDown = false;
    if (state.reconnectTimer !== undefined) clearTimeout(state.reconnectTimer);
    state.reconnectTimer = undefined;
    // Close the PREVIOUS session's channel and wait (bounded) before spawning: the successor
    // shares the seat's record, and a predecessor draining its queued frames after the new
    // session's seed would land stale state into fresh records.
    const previous = state.child;
    closeChild(previous);
    await awaitExit(previous, 2000);
    // The predecessor's cost belongs to the predecessor. The stash outlives session replacement
    // by design, so without this a replacement session's first frames would restate the old
    // session's cost as their own.
    if (reconnecting) {
      dropHeldForChannel();
    } else {
      state.reconnectAttempt = 0;
      state.lastCostUsd = undefined;
      state.pendingAskToolCallId = undefined;
      state.pendingApproval = false;
      resetHold();
      lastStateFrame = undefined;
      lastBackgroundJobs = undefined;
    }

    cancelSettle();
    const channelEnv: NodeJS.ProcessEnv = { ...process.env };
    if (runtimeId) channelEnv[RUNTIME_ID] = runtimeId;
    if (session) channelEnv[SESSION] = session;
    if (seq) channelEnv[SEQ] = seq;
    if (expectedNativeSession) {
      channelEnv[EXPECTED_NATIVE_SESSION] = expectedNativeSession;
    }
    if (resumeGeneration) channelEnv[RESUME_GENERATION] = resumeGeneration;
    const child = childProcess.spawn(
      bin,
      ["driver", "omp-channel", "--identity", identity],
      { stdio: ["pipe", "pipe", "inherit"], env: channelEnv },
    );
    state.child = child;
    state.todoFingerprint = undefined;
    state.todoBranchKey = undefined;
    state.todoLeafKey = undefined;
    state.todoNextPollAt = undefined;
    state.todoSession = nativeSessionId;
    state.todoReady = false;

    return new Promise<string>((resolve) => {
      let settled = false;
      let timedOut = false;
      const settle = (value: string) => {
        if (settled) return;
        settled = true;
        resolve(value);
      };
      const timer = setTimeout(() => { timedOut = true; settle(""); }, HELLO_TIMEOUT_MS);
      timer.unref?.();
      let legacyChannel = false;
      let keepalive: ReturnType<typeof setInterval> | undefined;
      const retire = () => {
        if (keepalive !== undefined) clearInterval(keepalive);
        if (state.child !== child) return;
        state.child = undefined;
        dropHeldForChannel();
        settle("");
        scheduleReconnect(ctx);
      };
      child.on("error", retire);
      // An observability pipe must never take the host down: EPIPE on a closed stdin is an
      // uncaught exception without a listener. Retire the channel instead — fail-open.
      child.stdin.on("error", retire);
      child.on("exit", retire);

      const send = (frame: Record<string, unknown>) => {
        if (child.stdin.destroyed) return;
        child.stdin.write(JSON.stringify(frame) + "\n");
      };
      send({ type: "session", sessionId: nativeSessionId });

      // Historical channels can fail their API request while Tokio still waits on a
      // blocking stdin read during shutdown. Wake that read so exit reaches the existing
      // reconnect handler. This frame carries no state, handoff or receipt authority.
      keepalive = setInterval(() => {
        if (state.child !== child || state.shuttingDown || child.stdin.destroyed) {
          if (keepalive !== undefined) clearInterval(keepalive);
          return;
        }
        if (lastStateFrame && backgroundJobs() !== lastBackgroundJobs) sendFrame(lastStateFrame);
        if (legacyChannel) send({ type: "keepalive" });
        if (state.todoReady) observeTodoBranch(ctx, false, true);
      }, 1000);
      keepalive.unref?.();

      const handle = async (line: string) => {
        let frame: Frame;
        try {
          frame = JSON.parse(line);
        } catch {
          return;
        }
        if (state.child !== child) return;
        if (frame.type === "settled" && typeof frame.meta?.messageId === "string") {
          // The daemon acknowledged read; the graph can no longer replay this native handoff.
          state.accepted?.delete(frame.meta.messageId);
          return;
        }
        if (frame.type === "seat" && frame.seat) {
          const seat = frame.seat;
          const label = seat.desired?.display_name ?? seat.member?.display_name ?? seat.subject?.replace(/^agent\//u, "");
          if (typeof label === "string") {
            state.label = seatLabel(label);
            await applyLabel(ctx);
          }
          return;
        }
        if (frame.type === "hello") {
          // A newer control plane may speak a wire this asset was not written against. Refusing
          // is the honest outcome: presence still decays, so the agent reads as unreachable
          // rather than silently never receiving mail.
          if (frame.protocol !== PROTOCOL && frame.protocol !== 1) {
            closeChild(child);
            ctx.ui?.notify?.(
              `st: omp channel protocol ${frame.protocol} is not understood by this extension (expected ${PROTOCOL}); reinstall st's hook set`,
            );
            settle("");
            return;
          }
          legacyChannel = frame.protocol === 1;
          state.reconnectAttempt = 0;
          await applyLabel(ctx);
          for (const accepted of state.accepted?.values() ?? []) {
            send({ type: "delivered", meta: accepted.meta });
            if (accepted.read) send({ type: "read", meta: accepted.meta });
          }
          send({ type: "ready", sessionId: nativeSessionId });
          state.todoReady = true;
          observeTodoBranch(ctx, true);
          // Every fresh channel needs the provider's idle proof, including reconnects
          // during an idle session where no further turn boundary will arrive.
          watchSettle(ctx);
          // The fence proves only the first session restored by this cold launch. A later explicit
          // in-process session switch becomes the current binding and must not inherit the old ID.
          expectedNativeSession = undefined;
          resumeGeneration = undefined;
          state.expectedNativeSession = undefined;
          state.resumeGeneration = undefined;
          clearTimeout(timer);
          const context = typeof frame.sessionContext === "string" ? frame.sessionContext : "";
          if (timedOut && context.trim()) {
            // A slow PTY projection may outlast the bounded session_start wait. Carry the seat
            // identity into the next turn instead of silently dropping the late hello.
            try {
              pi.sendMessage(
                { customType: "st-session-start", content: context, display: true },
                { deliverAs: "nextTurn" },
              );
            } catch { /* session shutdown may have begun */ }
          }
          settle(context);
          return;
        }
        if (frame.type !== "message" || typeof frame.content !== "string") return;
        const messageId = frame.meta?.messageId;
        if (typeof messageId === "string" && state.accepted?.has(messageId)) {
          const accepted = state.accepted.get(messageId)!;
          send({ type: "delivered", meta: accepted.meta });
          if (accepted.read) send({ type: "read", meta: accepted.meta });
          return;
        }
        await receive({
          content: frame.content,
          deliverAs: frame.deliverAs ?? "steer",
          meta: frame.meta,
          channel: child,
          ctx,
          reply: send,
        });
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

  const scheduleReconnect = (ctx: ExtensionContext) => {
    if (state.shuttingDown || state.reconnectTimer !== undefined) return;
    const attempt = Math.min((state.reconnectAttempt ?? 0) + 1, 6);
    state.reconnectAttempt = attempt;
    const delay = Math.min(500 * (2 ** (attempt - 1)), 5_000);
    state.reconnectTimer = setTimeout(() => {
      state.reconnectTimer = undefined;
      if (!state.shuttingDown && !state.child) void open(ctx, true);
    }, delay);
    state.reconnectTimer.unref?.();
  };

  // Frames are observational — st decides what becomes of them — and a closed channel drops
  // them silently.
  const sendFrame = (frame: Record<string, unknown>) => {
    const child = state.child;
    if (!child || !child.stdin || child.stdin.destroyed) return;
    if (frame.type === "state") {
      lastStateFrame = frame;
      lastBackgroundJobs = backgroundJobs();
      frame = { ...frame, backgroundJobs: lastBackgroundJobs };
    }
    child.stdin.write(JSON.stringify(frame) + "\n");
  };
  const emitTodo = (ctx: ExtensionContext, snapshot: TodoSnapshot, observedAt: string, sourceOp: string, force = false) => {
    if (!state.todoReady || !state.child || state.child.stdin?.destroyed) return;
    const nativeSession = ctx.sessionManager.getSessionId();
    if (state.todoSession !== nativeSession) return;
    const fingerprint = JSON.stringify(snapshot);
    if (!force && state.todoFingerprint === fingerprint) return;
    sendFrame({ type: "todo", session: nativeSession, observed_at: observedAt, source_op: sourceOp, ...snapshot });
    state.todoFingerprint = fingerprint;
  };
  const observeTodoBranch = (ctx: ExtensionContext, hydrate = false, polling = false) => {
    if (!state.todoReady || !state.child || state.child.stdin?.destroyed) return;
    const nativeSession = ctx.sessionManager.getSessionId();
    if (state.todoSession !== nativeSession) {
      if (!hydrate) return;
      // A branch hydration binds its observation provenance on the existing channel. It must
      // not restart delivery, discard held mail, or reset ask/approval authority.
      sendFrame({ type: "session", sessionId: nativeSession });
      state.todoSession = nativeSession;
      state.todoFingerprint = undefined;
      state.todoBranchKey = undefined;
      state.todoLeafKey = undefined;
    }
    let leafKey: string | undefined;
    try {
      // Native OMP exposes this O(1) index lookup on ReadonlySessionManager. /todo appends
      // user_todo_edit through appendCustomEntry, which advances the same branch leaf.
      if (typeof ctx.sessionManager.getLeafId === "function") {
        leafKey = `${nativeSession}:${ctx.sessionManager.getLeafId() ?? ""}`;
        if (!hydrate && state.todoLeafKey === leafKey) return;
      }
    } catch { return; }
    // Older providers without a leaf indicator still observe events immediately, but never
    // rebuild an unchanged idle branch every second.
    if (polling && leafKey === undefined && Date.now() < (state.todoNextPollAt ?? 0)) return;
    state.todoNextPollAt = Date.now() + 30_000;
    const source = branchTodo(ctx);
    if (source === undefined) return;
    state.todoLeafKey = leafKey;
    const key = source?.key ?? "";
    if (!hydrate && state.todoBranchKey === key) return;
    state.todoBranchKey = key;
    const snapshot = source?.snapshot ?? {
      phases: [], totals: { pending: 0, in_progress: 0, completed: 0, blocked: 0, abandoned: 0 }, truncated: false,
    };
    emitTodo(ctx, snapshot, source?.observedAt ?? new Date().toISOString(),
      hydrate ? "hydrate" : source?.sourceOp ?? "hydrate", hydrate);
  };
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
    // Per-response spend: the provider's disjoint token buckets, its model, and the cost the
    // harness itself computed. Nothing else about the request leaves the harness.
    const cost = usage?.cost && typeof usage.cost === "object"
      ? usage.cost as Record<string, unknown>
      : undefined;
    return { message: {
      id: boundedTimelineString(message.id, 256),
      role: boundedTimelineString(message.role, 32),
      content,
      model: boundedTimelineString(message.model, 128),
      provider: boundedTimelineString(message.provider, 64),
      usage: usage ? {
        input: finiteOrNull(usage.input ?? usage.inputTokens),
        output: finiteOrNull(usage.output ?? usage.outputTokens),
        cacheRead: finiteOrNull(usage.cacheRead),
        cacheWrite: finiteOrNull(usage.cacheWrite),
        totalTokens: finiteOrNull(usage.totalTokens),
        cost: cost ? finiteOrNull(cost.total) : null,
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

  // The idle edge without `agent_settled`: `ctx.isIdle()` is still false AT `agent_end` and
  // normally flips true within ~250ms. Slow unwind can exceed five seconds, so a timeout must
  // not abandon the only observer while st retains its last active frame. Keep sampling until
  // positive proof or newer activity; a queued follow-up keeps it false without an idle blip.
  const IDLE_POLL_MS = 100;

  const watchSettle = (ctx: ExtensionContext) => {
    if (state.pendingAskToolCallId || state.pendingApproval) return;
    // Starting a newer settle attempt also retires every older one.
    const generation = (state.settleGeneration ?? 0) + 1;
    state.settleGeneration = generation;
    // Bind this poll to the channel that was live when it started. A session
    // replacement inside the polling window would otherwise let a retired
    // context publish `idle` into the SUCCESSOR's channel while it is active.
    const originatingChild = state.child;
    const poller = setInterval(() => {
      if (
        state.child !== originatingChild ||
        state.settleGeneration !== generation ||
        state.pendingAskToolCallId ||
        state.pendingApproval
      ) {
        clearInterval(poller);
        return;
      }
      if (!idleProof(ctx)) return;
      clearInterval(poller);
      sendFrame({ type: "state", state: "idle" });
    }, IDLE_POLL_MS);
    poller.unref?.();
  };

  // Holding mail during a running turn; `HOLD_MAX_MS` explains why and where it is released.
  const toolCallsInFlight = () => (state.toolCallsInFlight ??= new Set<string>());
  const heldMessages = () => (state.held ??= []);
  const clearHoldTimer = () => {
    if (state.holdTimer !== undefined) clearTimeout(state.holdTimer);
    state.holdTimer = undefined;
  };
  /** Held mail belongs to the channel that sent it; that channel's successor re-sends it. */
  const dropHeldForChannel = () => {
    clearHoldTimer();
    state.held = [];
  };
  const resetHold = () => {
    dropHeldForChannel();
    state.running = false;
    toolCallsInFlight().clear();
  };

  // omp injects one queued steer per boundary (its default steering mode is `one-at-a-time`). A
  // second steer released at the same boundary waited through the next model turn and then
  // backgrounded that turn's whole batch (cross-omp-hold-luna-20260927-m), so mail released
  // together reaches omp as one message.
  const handOff = async (messages: HeldMessage[]) => {
    const [first] = messages;
    const content = messages.map((message) => message.content).join("\n\n");
    for (const message of messages) {
      const messageId = message.meta?.messageId;
      if (typeof messageId === "string") {
        state.accepted ??= new Map();
        state.accepted.set(messageId, { content: message.content, meta: message.meta });
      }
    }
    try {
      // `deliverAs` is required only while a turn is streaming, and an idle send that carries
      // one is rejected, so the idle proof selects the call shape. It never selects the
      // policy. Not optional-chained: reading a missing idle proof as "idle" would silently
      // turn every mid-turn delivery into a plain send.
      if (first.ctx.isIdle()) {
        await pi.sendUserMessage(content);
      } else {
        await pi.sendUserMessage(content, { deliverAs: first.deliverAs });
      }
      for (const message of messages) {
        message.reply({ type: "delivered", meta: message.meta });
      }
    } catch (error) {
      for (const message of messages) {
        const messageId = message.meta?.messageId;
        if (typeof messageId === "string" && state.accepted?.get(messageId)?.read) {
          message.reply({ type: "delivered", meta: message.meta });
          message.reply({ type: "read", meta: message.meta });
        } else {
          if (typeof messageId === "string") state.accepted?.delete(messageId);
          message.reply({ type: "failed", meta: message.meta, error: String(error) });
        }
      }
    }
  };

  /** Hand every held message to omp now, in arrival order. */
  const release = (): Promise<void> => {
    clearHoldTimer();
    const batch = heldMessages().splice(0);
    const deliver = async () => {
      const current = batch.filter((message) => message.channel === state.child);
      if (current.length > 0) await handOff(current);
    };
    state.handoff = (state.handoff ?? Promise.resolve()).then(deliver, deliver);
    return state.handoff;
  };

  /** Bound the wait behind a running tool call, never the wait for the model to finish streaming. */
  const armHoldCap = () => {
    if (state.holdTimer !== undefined) return;
    if (heldMessages().length === 0 || toolCallsInFlight().size === 0) return;
    state.holdTimer = setTimeout(() => {
      state.holdTimer = undefined;
      void release();
    }, HOLD_MAX_MS);
    state.holdTimer.unref?.();
  };

  /** Deliver a message from the channel now, or hold it while omp is running a turn. */
  const receive = (message: HeldMessage): Promise<void> => {
    heldMessages().push(message);
    if (state.running || toolCallsInFlight().size > 0) {
      armHoldCap();
      return Promise.resolve();
    }
    return release();
  };

  /** A turn's calls are over when the turn ends, answered or not: a blocked call has no result. */
  const endTurnToolCalls = (event: unknown) => {
    const content = (event as { message?: { content?: unknown } })?.message?.content;
    if (!Array.isArray(content)) return;
    const calls = toolCallsInFlight();
    for (const part of content) {
      if (!part || typeof part !== "object") continue;
      const { type, id } = part as { type?: unknown; id?: unknown };
      if (type === "toolCall" && typeof id === "string") calls.delete(id);
    }
    // Still held, now behind the model rather than a command: the next batch re-arms the cap.
    if (calls.size === 0) clearHoldTimer();
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
  // As on pi, this asset holds no cadence policy — st's write guard quantizes to 1% of the
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
   * cannot be read, which makes st count the edge itself: a weaker, incarnation-scoped answer
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

  // OMP loads extensions into subagent sessions that share this process-wide stash.
  // Keep delivery, receipt evidence and label authority on the top-level seat (#852).
  const isSubagent = (ctx: ExtensionContext | undefined): boolean =>
    (ctx as { agent?: { kind?: unknown } } | undefined)?.agent?.kind === "sub";
  const register = pi.on.bind(pi) as unknown as (
    event: string,
    handler: (event: unknown, ctx: ExtensionContext) => void | Promise<void>,
  ) => void;
  const onWidened = (
    event: string,
    handler: (event: unknown, ctx: ExtensionContext) => void | Promise<void>,
  ) => register(event, (payload, ctx) => {
    if (isSubagent(ctx)) return;
    jobContext = ctx;
    return handler(payload, ctx);
  });

  // Registered only now that every helper above is initialized: a use-before-declaration in this
  // file is the defect class that once shipped green through the type gate.
  onWidened("agent_start", async () => {
    cancelSettle();
    state.running = true;
    toolCallsInFlight().clear();
    sendFrame({ type: "state", state: "active" });
  });
  onWidened("agent_end", async (event, ctx) => {
    captureCost(event);
    sendContext(ctx);
    const end = event as AgentEndFrame;
    if (end.willContinue !== true) {
      state.running = false;
      toolCallsInFlight().clear();
      // Nothing runs now, so held mail is handed over at once, exactly as a message arriving at
      // this moment would be. Waiting longer for the idle proof would only delay it.
      if (heldMessages().length > 0) void release();
    }
    // A retried error does not end a turn: omp fires `agent_end` with `willContinue: true` for
    // every transient failure it is about to try again (measured: a 429 repeated seven times in
    // one print-mode run). Nothing is claimed from those — least of all about a credential.
    if (end.willContinue === true) {
      cancelSettle();
      return;
    }
    // The typed turn result, on every turn that ACTUALLY ended and in both directions. It is one
    // frame rather than two because the credential edge and the categorical state are the same
    // observation seen from two axes, and correlating them across frames would be a race st
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

  // The finest boundary that carries a fresh reading. Turn-boundary-only observation was measured
  // at 92% of pre-compaction warnings missed, because the wedge case is a single long turn.
  for (const name of ["message_end", "turn_end"]) {
    onWidened(name, async (event, ctx) => {
      captureCost(event);
      sendContext(ctx);
      if (name === "message_end") sendTimeline(name, event);
      if (name === "message_end") observeTodoBranch(ctx);
      if (name === "turn_end") endTurnToolCalls(event);
    });
  }
  // omp's `session_compact` carries NO `reason` and no `willRetry` — pi 0.84.2 has both — so the
  // trigger is withheld here and st records `unknown`. omp does name its auto-compaction "idle"
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
    sendTimeline("tool_call", rawEvent);
    if (typeof event.toolCallId === "string") {
      toolCallsInFlight().add(event.toolCallId);
      armHoldCap();
    }
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
    sendTimeline("tool_result", rawEvent);
    if (typeof event.toolCallId === "string") {
      const calls = toolCallsInFlight();
      calls.delete(event.toolCallId);
      // Awaited because omp awaits this handler: the steer is queued before omp looks for one at
      // this batch boundary, and after the batch's commands have run.
      if (calls.size === 0 && heldMessages().length > 0) await release();
    }
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

  onWidened("tool_execution_end", async (rawEvent, ctx) => {
    const event = record(rawEvent);
    if (event?.toolName !== "todo") {
      // Eval's native bridge persists successful nested todo calls as user_todo_edit entries.
      // This is also the human-edit path; neither needs result-text parsing.
      observeTodoBranch(ctx);
      return;
    }
    const result = record(event.result);
    const details = record(result?.details);
    if (event.isError === true || result?.isError === true ||
      typeof details?.op !== "string" || details.op === "view") return;
    const snapshot = boundedTodo(details.phases);
    if (!snapshot) return;
    state.todoBranchKey = branchTodo(ctx)?.key ?? "";
    try {
      state.todoLeafKey = typeof ctx.sessionManager.getLeafId === "function"
        ? `${ctx.sessionManager.getSessionId()}:${ctx.sessionManager.getLeafId() ?? ""}`
        : undefined;
    } catch { state.todoLeafKey = undefined; }
    emitTodo(ctx, snapshot, new Date().toISOString(), details.op);
  });

  // Approval events are an independent human-blocking surface. omp's pinned pi typings do not
  // declare them, so register through the same widened `on` view.
  type ApprovalFrame = { toolName?: unknown };
  onWidened("tool_approval_requested", async (rawEvent) => {
    state.pendingApproval = true;
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
    state.pendingApproval = false;
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

  pi.on("context", async (event, ctx) => {
    if (isSubagent(ctx)) return;
    observeTodoBranch(ctx);
    const texts = event.messages.flatMap((message) => {
      if (message.role !== "user") return [];
      return typeof message.content === "string" ? [message.content] : message.content.flatMap((part: { type: string; text?: string }) =>
        part.type === "text" && typeof part.text === "string" ? [part.text] : []);
    });
    for (const accepted of state.accepted?.values() ?? []) {
      if (!accepted.read && texts.some((text) => text.includes(accepted.content))) {
        accepted.read = true;
        sendFrame({ type: "delivered", meta: accepted.meta });
        sendFrame({ type: "read", meta: accepted.meta });
      }
    }
  });
  onWidened("session_switch", async (_event, ctx) => {
    await applyLabel(ctx);
    await open(ctx);
    await applyLabel(ctx);
  });
  for (const event of ["session_tree", "session_branch"]) {
    onWidened(event, async (_event, ctx) => {
      observeTodoBranch(ctx, true);
    });
  }
  onWidened("session_start", async (_event, ctx) => {
    // Awaited before the session's first turn, which is what makes restored context reach the boot
    // prompt rather than the turn after it.
    await applyLabel(ctx);
    const restored = await open(ctx);
    await applyLabel(ctx);
    const opened = state.child;
    if (opened) {
      // Seed the context record, so a resumed session publishes the window it resumed INTO
      // rather than waiting for its first turn boundary.
      sendContext(ctx);
    }
    if (restored.trim()) {
      // A custom message participates in LLM context without triggering a turn of its own.
      pi.sendMessage(
        { customType: "st-session-start", content: restored, display: true },
        { deliverAs: "nextTurn" },
      );
    }
  });
  // Upstream's `session_shutdown` payload has no reason: source defines it as process exit only.
  // Replacement is a separate session-switch lifecycle and remains handled by `open()` closing
  // the named predecessor.
  onWidened("session_shutdown", async () => {
    state.shuttingDown = true;
    if (state.reconnectTimer !== undefined) clearTimeout(state.reconnectTimer);
    state.reconnectTimer = undefined;
    cancelSettle();
    state.pendingAskToolCallId = undefined;
    state.pendingApproval = false;
    resetHold();
    closeChild(state.child);
  });
}
