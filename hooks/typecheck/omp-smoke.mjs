// Runtime smoke of the shipped omp extension: drives lifecycle, human-blocking, compaction, and
// terminal edges against a recorder standing in for the Rust channel. This catches both runtime
// defects a type-only gate cannot see and wire-contract regressions between the two languages.
import assert from "node:assert";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

// A recorder standing in for `st2 driver omp-channel`, so this smoke can assert what the extension
// EMITS and not merely that it loads — see smoke.mjs for why `true` as the channel binary cannot
// tell a working producer from one that writes nothing at all.
const dir = fs.mkdtempSync(path.join(os.tmpdir(), "st2-omp-smoke-"));
const framesPath = path.join(dir, "frames.jsonl");
const recorder = path.join(dir, "recorder");
fs.writeFileSync(
  recorder,
  `#!${process.execPath}
import fs from "node:fs";
process.stdout.write(JSON.stringify({ type: "hello", protocol: 1, sessionContext: "" }) + "\\n");
process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => fs.appendFileSync(${JSON.stringify(framesPath)}, chunk));
`,
  { mode: 0o755 },
);
const readFrames = () =>
  (fs.existsSync(framesPath) ? fs.readFileSync(framesPath, "utf8") : "")
    .split("\n")
    .filter((line) => line.trim())
    .map((line) => JSON.parse(line));

process.env.ST2_OMP_CHANNEL_BIN = recorder;
process.env.ST2_OMP_CHANNEL_CATALOG = "/tmp/st2-smoke-catalog";
process.env.ST2_OMP_CHANNEL_IDENTITY = "smoke.worker";
process.env.ST2_OMP_CHANNEL_RUNTIME_ID = "smoke.worker";
process.env.ST2_OMP_CHANNEL_SESSION = "smoke-session";
process.env.ST2_OMP_CHANNEL_SEQ = "1";

const mod = await import("./smoke-out/omp-channel.mjs");
assert.strictEqual(typeof mod.default, "function", "extension exports its entry point");

const handlers = new Map();
const pi = {
  on: (name, handler) => handlers.set(name, handler),
};
mod.default(pi);
for (const name of [
  "session_start",
  "session_before_compact",
  "session_shutdown",
  "agent_start",
  "agent_end",
  "tool_call",
  "tool_result",
  "tool_approval_requested",
  "tool_approval_resolved",
]) {
  assert.ok(handlers.has(name), `extension registers ${name}`);
}
assert.ok(!handlers.has("agent_settled"), "omp has no agent_settled event");
// The harness-context producer must be registered on the events it reads, or it observes nothing.
for (const name of ["message_end", "turn_end", "session_compact"]) {
  assert.ok(handlers.has(name), `extension registers ${name}`);
}

// A ctx with NOTHING the context producer wants: the fail-open half. An older omp build, or any
// ctx whose telemetry surface moved, must load and deliver mail exactly as before.
const bareCtx = {
  isIdle: () => true,
  ui: { notify: () => {} },
};
// A ctx carrying the surfaces measured on omp 18.0.9 (and reproduced on 18.0.3). `tokens` is the
// prompt figure — deliberately not this message's `totalTokens`. Without this ctx the producer's
// body never executes, and a use-before-declaration inside it would ship green through both the
// type gate and the old smoke.
const fullCtx = {
  ...bareCtx,
  model: { id: "fake-1", provider: "fakelab", contextWindow: 4000 },
  getContextUsage: () => ({ tokens: 22500, contextWindow: 4000, percent: 562.5 }),
  sessionManager: { getEntries: () => [{ type: "message" }, { type: "compaction" }] },
};
// And the hostile ctx: every telemetry pull throws. A guarded producer withholds; an unguarded one
// takes a turn down with it.
const throwingCtx = {
  ...bareCtx,
  get model() {
    throw new Error("smoke: model is not readable");
  },
  getContextUsage: () => {
    throw new Error("smoke: usage is not readable");
  },
  sessionManager: {
    getEntries: () => {
      throw new Error("smoke: entries are not readable");
    },
  },
};

const messageEvent = {
  message: {
    role: "assistant",
    usage: { input: 22400, output: 25, totalTokens: 22525, cost: { total: 0.067605 } },
  },
};

for (const ctx of [bareCtx, fullCtx, throwingCtx]) {
  // Two session starts in a row: the second exercises the predecessor close-and-await path.
  await handlers.get("session_start")({}, ctx);
  await handlers.get("session_start")({}, ctx);
  await handlers.get("tool_approval_requested")({ toolName: "bash" }, ctx);
  await handlers.get("tool_approval_resolved")({ approved: true }, ctx);
  await handlers.get("agent_start")({}, ctx);
  await handlers.get("message_end")(messageEvent, ctx);
  await handlers.get("turn_end")(messageEvent, ctx);
  await handlers.get("agent_end")(messageEvent, ctx);
  // omp's event names no reason: the producer must withhold the trigger, never invent one.
  await handlers.get("session_compact")({ compactionEntry: { id: "86c8955c" } }, ctx);
  await handlers.get("session_before_compact")({}, ctx);
  await handlers.get("session_shutdown")({}, ctx);
}

// Structured ask observation is correlated by toolCallId. An unrelated result must emit nothing
// (and therefore leave the durable blocked frame intact); only the matching result clears it.
const activeCtx = { ...fullCtx, isIdle: () => false };
await handlers.get("session_start")({}, activeCtx);
await new Promise((resolve) => setTimeout(resolve, 50));
const beforeAsk = readFrames().filter((frame) => frame.type === "state").length;
await handlers.get("tool_call")(
  {
    toolName: "ask",
    toolCallId: "ask-1",
    input: {
      questions: [{ id: "target", question: "  Which\n deployment target?  ", options: [] }],
    },
  },
  activeCtx,
);
await handlers.get("tool_result")({ toolName: "read", toolCallId: "unrelated" }, activeCtx);
await new Promise((resolve) => setTimeout(resolve, 50));
let askStates = readFrames().filter((frame) => frame.type === "state").slice(beforeAsk);
assert.deepStrictEqual(askStates, [
  {
    type: "state",
    state: "active",
    blockedOn: "human",
    ask: "question",
    reason: "Which deployment target?",
  },
]);
await handlers.get("tool_result")({ toolName: "ask", toolCallId: "ask-1" }, activeCtx);
await new Promise((resolve) => setTimeout(resolve, 50));
askStates = readFrames().filter((frame) => frame.type === "state").slice(beforeAsk);
assert.deepStrictEqual(askStates.at(-1), { type: "state", state: "active" });

// Every poll is generation-fenced. New activity, an automatic continuation, and a terminal error
// each retire an older settle poll before it can publish a stale idle frame.
let settleIdle = false;
const settleCtx = { ...fullCtx, isIdle: () => settleIdle };
const successfulEnd = {
  messages: [{ role: "assistant", stopReason: "stop" }],
};

let beforeSettleCase = readFrames().filter((frame) => frame.type === "state").length;
await handlers.get("agent_end")(successfulEnd, settleCtx);
await handlers.get("agent_start")({}, settleCtx);
settleIdle = true;
await new Promise((resolve) => setTimeout(resolve, 250));
assert.deepStrictEqual(
  readFrames().filter((frame) => frame.type === "state").slice(beforeSettleCase),
  [{ type: "state", state: "active" }],
  "new activity must cancel the older settle poll",
);

settleIdle = false;
beforeSettleCase = readFrames().filter((frame) => frame.type === "state").length;
let beforeTurns = readFrames().filter((frame) => frame.type === "turn").length;
await handlers.get("agent_end")(successfulEnd, settleCtx);
await handlers.get("agent_end")(
  {
    willContinue: true,
    messages: [{ role: "assistant", stopReason: "error", errorMessage: "transient retry" }],
  },
  settleCtx,
);
settleIdle = true;
await new Promise((resolve) => setTimeout(resolve, 250));
assert.strictEqual(
  readFrames().filter((frame) => frame.type === "state").length,
  beforeSettleCase,
  "willContinue must cancel the older poll and start no new settle",
);
// A retried turn has not ended, so it claims neither credential edge: only the ordinary end
// before it may emit a turn result.
assert.deepStrictEqual(
  readFrames().filter((frame) => frame.type === "turn").slice(beforeTurns),
  [{ type: "turn" }],
  "willContinue must emit no turn result",
);

settleIdle = false;
beforeSettleCase = readFrames().filter((frame) => frame.type === "state").length;
beforeTurns = readFrames().filter((frame) => frame.type === "turn").length;
await handlers.get("agent_end")(successfulEnd, settleCtx);
await handlers.get("agent_end")(
  {
    messages: [
      { role: "user" },
      {
        role: "assistant",
        stopReason: "error",
        errorMessage: "  credential\n expired  ",
        errorStatus: 401,
        errorId: 16781312,
      },
    ],
  },
  settleCtx,
);
settleIdle = true;
await new Promise((resolve) => setTimeout(resolve, 250));
// The terminal error's whole observation rides ONE frame: the typed turn result. It cancels the
// older settle poll, so no stale idle lands, and it carries omp's own classification bitfield
// verbatim — st2, not this asset, decides whether that names a rejected credential. `errorStatus`
// stays off the wire on purpose.
assert.strictEqual(
  readFrames().filter((frame) => frame.type === "state").length,
  beforeSettleCase,
  "terminal error must cancel the older poll without asserting a state word",
);
assert.deepStrictEqual(
  readFrames().filter((frame) => frame.type === "turn").slice(beforeTurns),
  [
    { type: "turn" },
    { type: "turn", error: { reason: "credential expired", errorId: 16781312 } },
  ],
  "terminal error must emit the typed turn result and stay actionable",
);

// `session_shutdown` has no reason field upstream and always denotes process exit. Closing must
// make a later observational frame a no-op.
const beforeShutdown = readFrames().filter((frame) => frame.type === "state").length;
await handlers.get("session_shutdown")({}, fullCtx);
await handlers.get("agent_start")({}, fullCtx);
await new Promise((resolve) => setTimeout(resolve, 50));
assert.strictEqual(
  readFrames().filter((frame) => frame.type === "state").length,
  beforeShutdown,
  "shutdown without a reason closes the channel",
);

// Give the recorder a moment to drain, then assert the wire the Rust decoder reads.
await new Promise((resolve) => setTimeout(resolve, 500));
const frames = readFrames();
assert.ok(
  frames.some((frame) => frame.type === "pre_compact"),
  "session_before_compact must emit the Rust-owned recovery edge",
);
const context = frames.filter((frame) => frame.type === "context");
assert.ok(context.length > 0, "the producer must emit context frames, not merely load");

const reading = context.find((frame) => frame.reading);
assert.ok(reading, "a context frame must carry a `reading` object");
// Every leg is always present, `null` where withheld — one absence convention on the wire.
for (const key of ["usedTokens", "windowTokens", "usedPercent", "model", "costUsd"]) {
  assert.ok(key in reading.reading, `reading carries ${key}`);
}

// Selected by predicate, not by position. The version-coupled constant is asserted on the wire as
// well as in the Rust fixture: omp's numerator is the prompt figure, never this message's
// totalTokens (22525), and the two would be indistinguishable in a round-trip test.
const known = context.find((frame) => typeof frame.reading?.usedTokens === "number");
assert.ok(known, "a populated context must produce a reading with real numbers");
assert.strictEqual(known.reading.usedTokens, 22500, "omp's numerator is the prompt figure");
assert.notStrictEqual(known.reading.usedTokens, 22525, "publishing totalTokens would be pi's rule");
assert.strictEqual(known.reading.usedPercent, 562.5, "carried raw, never clamped");

const edges = context.filter((frame) => frame.compaction);
assert.ok(edges.length > 0, "a compaction edge must ride a context frame");
// omp's event names no reason, so the producer withholds rather than inventing one — on every
// edge, from every context. st2 records `unknown`.
for (const edge of edges) {
  assert.strictEqual(edge.compaction.trigger, null, "omp names no trigger");
}
assert.ok(
  edges.some((edge) => edge.compaction.count === null),
  "an unreadable session store must still send the edge, countless",
);
const durable = edges.find((edge) => typeof edge.compaction.count === "number");
assert.ok(durable, "a readable session store must supply the durable count");
assert.strictEqual(durable.compaction.count, 1, "the count is getEntries() filtered to compactions");
// Unlike pi, omp still answers inside its own compact handler, so a real reading rides the edge.
assert.strictEqual(durable.reading.usedTokens, 22500, "omp does not null its reading at the edge");

fs.rmSync(dir, { recursive: true, force: true });
console.log("omp extension smoke: ok");
process.exit(0);
