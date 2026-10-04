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
// Lines appended here are what the channel sends the extension, as `message` frames do.
const outboxPath = path.join(dir, "outbox.jsonl");
const pidPath = path.join(dir, "channel-pids");
const delayedHelloPath = path.join(dir, "delay-hello");
const recorder = path.join(dir, "recorder");
fs.writeFileSync(
  recorder,
  `#!${process.execPath}
import fs from "node:fs";
fs.appendFileSync(${JSON.stringify(pidPath)}, process.pid + "\\n");
const hello = () => process.stdout.write(JSON.stringify({ type: "hello", protocol: 1, sessionContext: fs.existsSync(${JSON.stringify(delayedHelloPath)}) ? "late seat context" : "" }) + "\\n");
if (fs.existsSync(${JSON.stringify(delayedHelloPath)})) setTimeout(hello, 6000);
else hello();
process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => fs.appendFileSync(${JSON.stringify(framesPath)}, chunk));
process.stdin.on("end", () => process.exit(0));
let sent = 0;
setInterval(() => {
  let outbox = "";
  try {
    outbox = fs.readFileSync(${JSON.stringify(outboxPath)}, "utf8");
  } catch {
    return;
  }
  if (outbox.length > sent) {
    process.stdout.write(outbox.slice(sent));
    sent = outbox.length;
  }
}, 10);
`,
  { mode: 0o755 },
);
const readFrames = () =>
  (fs.existsSync(framesPath) ? fs.readFileSync(framesPath, "utf8") : "")
    .split("\n")
    .filter((line) => line.trim())
    .map((line) => JSON.parse(line));

process.env.ST_OMP_CHANNEL_BIN = recorder;
process.env.ST_OMP_CHANNEL_CATALOG = "/tmp/st2-smoke-catalog";
process.env.ST_OMP_CHANNEL_IDENTITY = "smoke.worker";
process.env.ST_OMP_CHANNEL_RUNTIME_ID = "smoke.worker";
process.env.ST_OMP_CHANNEL_SESSION = "smoke-session";
process.env.ST_OMP_CHANNEL_SEQ = "1";

const mod = await import(process.argv[2] ?? "./smoke-out/omp-channel.mjs");
assert.strictEqual(typeof mod.default, "function", "extension exports its entry point");

const expectedState = state => ({ type: "state", state,
  ...(process.argv[2]?.includes("st-omp") ? { backgroundJobs: null } : {}),
});
const handlers = new Map();
// Every message the extension hands to omp, with the options it chose.
const handedOver = [];
const sessionMessages = [];
const pi = {
  on: (name, handler) => handlers.set(name, handler),
  sendUserMessage: (content, options) => {
    handedOver.push(options === undefined ? { content } : { content, options });
  },
  sendMessage: (message, options) => sessionMessages.push({ message, options }),
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
  sessionManager: { getSessionId: () => "session-smoke" },
};
// A ctx carrying the surfaces measured on omp 18.0.9 (and reproduced on 18.0.3). `tokens` is the
// prompt figure — deliberately not this message's `totalTokens`. Without this ctx the producer's
// body never executes, and a use-before-declaration inside it would ship green through both the
// type gate and the old smoke.
const fullCtx = {
  ...bareCtx,
  model: { id: "fake-1", provider: "fakelab", contextWindow: 4000 },
  getContextUsage: () => ({ tokens: 22500, contextWindow: 4000, percent: 562.5 }),
  sessionManager: {
    getSessionId: () => "session-smoke",
    getEntries: () => [{ type: "message" }, { type: "compaction" }],
  },
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
    getSessionId: () => "session-smoke",
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
let askStates = readFrames()
  .filter((frame) => frame.type === "state")
  .slice(beforeAsk);
assert.deepStrictEqual(askStates, [
  {
    ...expectedState("active"),
    blockedOn: "human",
    ask: "question",
    reason: "Which deployment target?",
  },
]);
await handlers.get("tool_result")({ toolName: "ask", toolCallId: "ask-1" }, activeCtx);
await new Promise((resolve) => setTimeout(resolve, 50));
askStates = readFrames().filter((frame) => frame.type === "state").slice(beforeAsk);
assert.deepStrictEqual(askStates.at(-1), expectedState("active"));

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
  [expectedState("active")],
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

// A slow final unwind must not strand a working projection after the old five-second
// settle window. Drive the actual extension and inspect the channel's observed idle edge.
settleIdle = false;
await handlers.get("agent_start")({}, settleCtx);
await handlers.get("agent_end")(successfulEnd, settleCtx);
await new Promise((resolve) => setTimeout(resolve, 5250));
beforeSettleCase = readFrames().filter((frame) => frame.type === "state").length;
settleIdle = true;
await new Promise((resolve) => setTimeout(resolve, 250));
assert.deepStrictEqual(
  readFrames().filter((frame) => frame.type === "state").slice(beforeSettleCase),
  [expectedState("idle")],
  "native idle after a slow unwind must replace working without another turn",
);

// Session replacement fences the retained observer: an old context becoming idle
// cannot overwrite the new session's genuinely busy state.
settleIdle = false;
await handlers.get("agent_end")(successfulEnd, settleCtx);
await handlers.get("session_start")({}, activeCtx);
await handlers.get("agent_start")({}, activeCtx);
await new Promise((resolve) => setTimeout(resolve, 50));
beforeSettleCase = readFrames().filter((frame) => frame.type === "state").length;
settleIdle = true;
await new Promise((resolve) => setTimeout(resolve, 250));
assert.deepStrictEqual(
  readFrames().filter((frame) => frame.type === "state").slice(beforeSettleCase),
  [],
  "a retired context cannot mark a busy successor idle",
);

// Mail that arrives while omp runs a turn is held until the tool batch's last result, where omp
// injects a steer anyway, so omp never backgrounds a command for it (`HOLD_MAX_MS` in
// omp-channel.ts). Each case sends message frames through the channel and reads what the extension
// hands to omp and what it acknowledges.
let holdIdle = false;
const holdCtx = { ...fullCtx, isIdle: () => holdIdle };
const pause = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
let mailSequence = 0;
const sendMail = (content) => {
  mailSequence += 1;
  const frame = {
    type: "message",
    deliverAs: "steer",
    content,
    meta: { messageId: `message/hold-${mailSequence}` },
  };
  fs.appendFileSync(outboxPath, JSON.stringify(frame) + "\n");
};
const acknowledged = () =>
  readFrames()
    .filter((frame) => frame.type === "delivered")
    .map((frame) => frame.meta?.messageId);
const toolCall = (id) =>
  handlers.get("tool_call")({ toolName: "bash", toolCallId: id, input: {} }, holdCtx);
const toolResult = (id) =>
  handlers.get("tool_result")({ toolName: "bash", toolCallId: id }, holdCtx);
await handlers.get("session_start")({}, holdCtx);

// Measured failure: a steer that arrived while the model streamed backgrounded the next batch's
// commands as they started. Mail now waits for the whole batch and is steered as its last call
// returns, inside the awaited handler, so omp finds it at that same boundary. omp injects one
// queued steer per boundary, so mail released together is one steer: a second one would wait
// through the next model turn and background that turn's batch.
await handlers.get("agent_start")({}, holdCtx);
sendMail("while streaming");
sendMail("also while streaming");
await pause(200);
assert.deepStrictEqual(handedOver, [], "mail during a running turn is held, not steered at once");
await toolCall("batch-a");
await toolCall("batch-b");
await toolResult("batch-a");
await pause(50);
assert.deepStrictEqual(handedOver, [], "held while any call of the batch is in flight");
await toolResult("batch-b");
assert.deepStrictEqual(
  handedOver,
  [{ content: "while streaming\n\nalso while streaming", options: { deliverAs: "steer" } }],
  "one steer by the time the batch's last tool_result handler returns",
);
await pause(100);
assert.deepStrictEqual(
  acknowledged(),
  ["message/hold-1", "message/hold-2"],
  "each message is acknowledged once omp has it",
);

// Mail held to the end of a run is handed over at `agent_end`, exactly as a message arriving then
// would be. This context is not yet idle there, so it is a steer.
sendMail("during the final answer");
await pause(200);
assert.strictEqual(handedOver.length, 1, "held while the model writes its final answer");
await handlers.get("agent_end")(successfulEnd, holdCtx);
await pause(20);
assert.deepStrictEqual(handedOver.at(-1), {
  content: "during the final answer",
  options: { deliverAs: "steer" },
});

holdIdle = true;
sendMail("while idle");
await pause(200);
assert.deepStrictEqual(handedOver.at(-1), { content: "while idle" }, "idle mail is not held");

// A long command delays mail by at most the cap. Then it is steered as before, and omp
// backgrounds the command.
holdIdle = false;
await handlers.get("agent_start")({}, holdCtx);
await toolCall("long-command");
sendMail("behind a long command");
await pause(9_000);
assert.strictEqual(handedOver.length, 3, "held behind a running command until the cap");
await pause(1_500);
assert.deepStrictEqual(
  handedOver.at(-1),
  { content: "behind a long command", options: { deliverAs: "steer" } },
  "released by the cap",
);
await toolResult("long-command");
await handlers.get("agent_end")(successfulEnd, holdCtx);
await pause(100);
assert.deepStrictEqual(
  acknowledged(),
  ["message/hold-1", "message/hold-2", "message/hold-3", "message/hold-4", "message/hold-5"],
  "every message is acknowledged exactly once",
);
fs.rmSync(outboxPath, { force: true });

// A channel that dies during a hold must be replaced. The successor re-sends the unacknowledged
// message; the retired channel's held copy must never be handed off or acknowledged.
await handlers.get("session_start")({}, holdCtx);
await handlers.get("agent_start")({}, holdCtx);
const reheld = { type: "message", deliverAs: "steer", content: "after reconnect", meta: { messageId: "message/reopen" } };
fs.appendFileSync(outboxPath, JSON.stringify(reheld) + "\n");
await pause(200);
const beforeReopen = handedOver.length;
const oldPid = Number(fs.readFileSync(pidPath, "utf8").trim().split("\n").at(-1));
fs.writeFileSync(outboxPath, "");
process.kill(oldPid);
let newPid = oldPid;
for (let i = 0; i < 30 && newPid === oldPid; i++) {
  await pause(100);
  newPid = Number(fs.readFileSync(pidPath, "utf8").trim().split("\n").at(-1));
}
assert.notStrictEqual(newPid, oldPid, "the channel reopens after an unexpected exit");
fs.appendFileSync(outboxPath, JSON.stringify(reheld) + "\n");
await pause(200);
assert.strictEqual(handedOver.length, beforeReopen, "successor mail is still held during the turn");
await handlers.get("agent_end")(successfulEnd, holdCtx);
await pause(100);
assert.strictEqual(acknowledged().filter((id) => id === "message/reopen").length, 1);
fs.rmSync(outboxPath, { force: true });

// An idle reconnect has no future agent_end to establish readiness. The replacement
// must receive a fresh idle proof from the same provider, without a synthetic turn.
await handlers.get("session_start")({}, bareCtx);
await pause(150);
const idlePid = Number(fs.readFileSync(pidPath, "utf8").trim().split("\n").at(-1));
const idleFramesBefore = readFrames().filter((frame) => frame.type === "state" && frame.state === "idle").length;
process.kill(idlePid, "SIGKILL");
await pause(1000);
assert.notStrictEqual(Number(fs.readFileSync(pidPath, "utf8").trim().split("\n").at(-1)), idlePid);
assert.ok(readFrames().filter((frame) => frame.type === "state" && frame.state === "idle").length > idleFramesBefore,
  "a replacement channel receives readiness even when no model turn runs");

// Reconnection may sample a context that reports idle while its modal is still
// awaiting the operator. Ask and approval authority must survive that sample.
for (const modal of ["ask", "approval"]) {
  await handlers.get("session_start")({}, bareCtx);
  await pause(150);
  if (modal === "ask") {
    await handlers.get("tool_call")({
      toolName: "ask",
      toolCallId: "ask-reconnect",
      input: { questions: [{ question: "Proceed?" }] },
    }, bareCtx);
  } else {
    await handlers.get("tool_approval_requested")({ toolName: "bash" }, bareCtx);
  }
  await pause(50);
  const modalPid = Number(fs.readFileSync(pidPath, "utf8").trim().split("\n").at(-1));
  const beforeModalReconnect = readFrames().filter((frame) => frame.type === "state").length;
  const readyBeforeModalReconnect = readFrames().filter((frame) => frame.type === "ready").length;
  process.kill(modalPid, "SIGKILL");
  for (let i = 0; i < 50 &&
    readFrames().filter((frame) => frame.type === "ready").length === readyBeforeModalReconnect; i++) {
    await pause(100);
  }
  assert.strictEqual(readFrames().filter((frame) => frame.type === "ready").length,
    readyBeforeModalReconnect + 1, "the modal's replacement channel completes its hello");
  await pause(250);
  assert.notStrictEqual(Number(fs.readFileSync(pidPath, "utf8").trim().split("\n").at(-1)), modalPid);
  assert.deepStrictEqual(
    readFrames().filter((frame) => frame.type === "state").slice(beforeModalReconnect),
    [],
    `${modal} reconnect must not fabricate idle while waiting for the operator`,
  );
  if (modal === "ask") {
    await handlers.get("tool_result")({ toolCallId: "ask-reconnect" }, bareCtx);
  } else {
    await handlers.get("tool_approval_resolved")({}, bareCtx);
  }
  await pause(50);
  assert.deepStrictEqual(
    readFrames().filter((frame) => frame.type === "state").at(-1),
    expectedState("idle"),
    `${modal} resolution permits a positive idle observation`,
  );
}

// The channel may answer after session_start's bounded wait. Its seat context still reaches the
// next turn, so a slow PTY projection cannot leave an unnamed seat.
fs.writeFileSync(delayedHelloPath, "1");
await handlers.get("session_start")({}, fullCtx);
await pause(1500);
assert.ok(sessionMessages.some(({ message }) => message.content === "late seat context"));
fs.rmSync(delayedHelloPath, { force: true });

// omp loads this extension into every in-process subagent, as a second instance sharing the
// process-wide stash (compoundingtech/smalltalk#852). A subagent's lifecycle must leave the seat's
// channel alone: no channel of its own, no frames from its turns, and its shutdown does not close
// the seat's channel. Mail that arrives afterwards still reaches the top-level session.
const subHandlers = new Map();
const subHandedOver = [];
mod.default({
  on: (name, handler) => subHandlers.set(name, handler),
  sendUserMessage: (content) => subHandedOver.push(content),
  sendMessage: () => {},
});
const subCtx = {
  ...fullCtx,
  isIdle: () => false,
  agent: { kind: "sub", id: "0-Review", name: "task", depth: 1, parentId: "Main" },
  sessionManager: { getSessionId: () => "session-subagent", getEntries: () => [] },
};
const pidsBeforeSubagent = fs.readFileSync(pidPath, "utf8");
const framesBeforeSubagent = readFrames().length;
await subHandlers.get("session_start")({}, subCtx);
await subHandlers.get("agent_start")({}, subCtx);
await subHandlers.get("tool_call")({ toolName: "bash", toolCallId: "sub-call", input: {} }, subCtx);
await subHandlers.get("message_end")(messageEvent, subCtx);
await subHandlers.get("agent_end")(successfulEnd, subCtx);
await subHandlers.get("session_shutdown")({}, subCtx);
await pause(300);
assert.strictEqual(fs.readFileSync(pidPath, "utf8"), pidsBeforeSubagent, "a subagent opens no channel");
assert.deepStrictEqual(readFrames().slice(framesBeforeSubagent), [], "a subagent's events reach no channel");
// Still mid-turn from the subagent's point of view; the seat's session is idle, so mail goes now.
fs.appendFileSync(outboxPath, JSON.stringify({
  type: "message",
  deliverAs: "steer",
  content: "after a subagent",
  meta: { messageId: "message/after-subagent" },
}) + "\n");
await pause(300);
assert.deepStrictEqual(handedOver.at(-1), { content: "after a subagent" }, "the seat's session gets the mail");
assert.deepStrictEqual(subHandedOver, [], "the subagent never gets the seat's mail");
assert.ok(acknowledged().includes("message/after-subagent"));
// The top-level session names itself `main` on omp 18.3.2 and later; its events still flow.
const mainCtx = { ...fullCtx, agent: { kind: "main", id: "Main", name: "main", depth: 0 } };
const framesBeforeMain = readFrames().length;
await handlers.get("agent_start")({}, mainCtx);
await pause(50);
assert.deepStrictEqual(readFrames().slice(framesBeforeMain), [expectedState("active")]);
await handlers.get("agent_end")(successfulEnd, mainCtx);
fs.rmSync(outboxPath, { force: true });

// st3's dedicated todo observation is deliberately absent from the legacy st2 asset.
if (process.argv[2]?.includes("st-omp-channel") || process.argv.includes("--todo")) {
  const totals = (pending = 0, in_progress = 0, completed = 0, blocked = 0, abandoned = 0) =>
    ({ pending, in_progress, completed, blocked, abandoned });
  const todos = () => readFrames().filter((frame) => frame.type === "todo");
  const sourceTime = "2026-10-03T12:00:00.000Z";
  const phase = (status = "in_progress", content = "Review", blocker) => [{
    name: "Work", tasks: [{ content, status, ...(blocker === undefined ? {} : { blocker }) }],
  }];
  const toolEntry = (id, op, phases, isError = false) => ({
    id, type: "message", timestamp: sourceTime,
    message: { role: "toolResult", toolName: "todo", isError, details: { op, phases } },
  });
  let branch = [
    toolEntry("init", "init", phase()),
    toolEntry("failed", "done", phase("completed"), true),
    toolEntry("view", "view", phase("completed")),
    { id: "unstructured", type: "message", timestamp: sourceTime,
      message: { role: "toolResult", toolName: "todo", details: { op: "done" } } },
  ];
  let nativeSession = "session-todo";
  let branchReads = 0;
  const todoCtx = {
    ...bareCtx,
    sessionManager: {
      getSessionId: () => nativeSession,
      getLeafId: () => branch.at(-1)?.id ?? null,
      getBranch: () => { branchReads++; return branch; },
      // A newer result on a different branch MUST NOT seed this binding.
      getEntries: () => [...branch, toolEntry("unrelated", "block", phase("blocked", "Wrong branch", "No"))],
    },
  };
  const startCount = todos().length;
  await handlers.get("session_start")({}, todoCtx);
  await pause(50);
  assert.deepStrictEqual(todos().slice(startCount), [{
    type: "todo", session: nativeSession, source_op: "hydrate", observed_at: sourceTime,
    phases: phase(), totals: totals(0, 1), truncated: false,
  }], "hydrate newest successful structured non-view result from current branch");
  const firstTodo = readFrames().findIndex((frame) => frame.type === "todo" && frame.session === nativeSession);
  assert.ok(readFrames().slice(0, firstTodo).some((frame) => frame.type === "ready" && frame.sessionId === nativeSession));
  const execute = async (op, phases, flags = {}) => handlers.get("tool_execution_end")({
    toolName: "todo", toolCallId: `todo-${op}`, result: { details: { op, phases } }, ...flags,
  }, todoCtx);
  await execute("done", phase("completed"), { isError: true });
  await execute("done", phase("completed"), {
    result: { isError: true, details: { op: "done", phases: phase("completed") } },
  });
  await execute("view", phase("completed"));
  await execute("start", phase());
  await pause(50);
  assert.strictEqual(todos().length, startCount + 1, "error/view/unchanged mutations do not replace snapshot");
  branch.push(toolEntry("done", "done", phase("completed")));
  await execute("done", phase("completed"));
  await pause(50);
  assert.deepStrictEqual(todos().at(-1).totals, totals(0, 0, 1));
  assert.strictEqual(todos().at(-1).source_op, "done");
  assert.ok(Number.isFinite(Date.parse(todos().at(-1).observed_at)));
  assert.strictEqual(todos().at(-1).session, nativeSession);
  branch.push(toolEntry("block", "block", phase("blocked", "Review", "Waiting for approval")));
  await execute("block", phase("blocked", "Review", "Waiting for approval"));
  await pause(50);
  assert.deepStrictEqual(todos().at(-1).phases, phase("blocked", "Review", "Waiting for approval"));

  const readsBeforeIdle = branchReads;
  await pause(2100);
  assert.strictEqual(branchReads, readsBeforeIdle, "unchanged idle leaf never rebuilds the branch");
  // /todo writes a custom entry without emitting an extension event. Polling its O(1) leaf
  // indicator must observe it even while idle, including an explicit clear.
  branch.push({ id: "human", type: "custom", customType: "user_todo_edit", timestamp: sourceTime,
    data: { phases: phase("pending", "Human task") } });
  await pause(1100);
  assert.strictEqual(todos().at(-1).source_op, "user_edit");
  assert.deepStrictEqual(todos().at(-1).phases, phase("pending", "Human task"));
  branch.push({ id: "human-clear", type: "custom", customType: "user_todo_edit", timestamp: sourceTime,
    data: { phases: [] } });
  await pause(1100);
  assert.deepStrictEqual(todos().at(-1).phases, []);
  assert.deepStrictEqual(todos().at(-1).totals, totals());
  assert.strictEqual(todos().at(-1).truncated, false);
  await handlers.get("session_start")({}, todoCtx);
  await pause(50);
  assert.strictEqual(todos().at(-1).source_op, "hydrate", "clear survives a new binding");
  assert.strictEqual(todos().at(-1).observed_at, sourceTime);

  branch = [toolEntry("older-branch", "init", phase("pending", "Branch task"))];
  await handlers.get("session_tree")({}, todoCtx);
  await pause(50);
  assert.deepStrictEqual(todos().at(-1).phases, phase("pending", "Branch task"));
  assert.strictEqual(todos().at(-1).source_op, "hydrate");
  branch = [];
  await handlers.get("session_tree")({}, todoCtx);
  await pause(50);
  assert.deepStrictEqual(todos().at(-1).phases, [], "no branch snapshot clears another branch's work");
  await handlers.get("agent_start")({}, todoCtx);
  const beforeBranchMail = handedOver.length;
  fs.appendFileSync(outboxPath, JSON.stringify({
    type: "message", deliverAs: "steer", content: "held across branch",
    meta: { messageId: "message/todo-branch" },
  }) + "\n");
  await pause(100);
  assert.strictEqual(handedOver.length, beforeBranchMail, "branch fixture has a held delivery");
  const pidsBeforeBranch = fs.readFileSync(pidPath, "utf8");
  nativeSession = "session-todo-next";
  const beforeNewSession = readFrames().length;
  await handlers.get("session_branch")({}, todoCtx);
  await pause(50);
  const newSessionFrames = readFrames().slice(beforeNewSession);
  assert.strictEqual(newSessionFrames[0].type, "session");
  assert.strictEqual(newSessionFrames[0].sessionId, nativeSession);
  assert.strictEqual(todos().at(-1).session, nativeSession);
  assert.deepStrictEqual(todos().at(-1).totals, totals());
  assert.strictEqual(fs.readFileSync(pidPath, "utf8"), pidsBeforeBranch, "branch hydration does not reopen delivery");
  assert.ok(!newSessionFrames.some((frame) => frame.type === "ready"), "branch hydration does not rerun delivery readiness");
  assert.strictEqual(handedOver.length, beforeBranchMail, "branch hydration preserves held mail");
  await handlers.get("agent_end")(successfulEnd, todoCtx);
  await pause(100);
  assert.deepStrictEqual(handedOver.at(-1), {
    content: "held across branch",
  }, "held mail survives native-session provenance binding");
  assert.strictEqual(acknowledged().filter((id) => id === "message/todo-branch").length, 1);
  fs.rmSync(outboxPath, { force: true });

  // OMP 18.4.4 eval/js/tool-bridge persists committed nested todo calls through the
  // same user_todo_edit path; the enclosing eval event need not expose nested results.
  branch.push({ id: "eval-todo", type: "custom", customType: "user_todo_edit", timestamp: sourceTime,
    data: { phases: phase("in_progress", "Nested eval task") } });
  await handlers.get("tool_execution_end")({ toolName: "eval", result: {} }, todoCtx);
  await pause(50);
  assert.deepStrictEqual(todos().at(-1).phases, phase("in_progress", "Nested eval task"));
  const beforeReconnect = todos().length;
  const todoPid = Number(fs.readFileSync(pidPath, "utf8").trim().split("\n").at(-1));
  process.kill(todoPid, "SIGKILL");
  await pause(1100);
  assert.strictEqual(todos().length, beforeReconnect + 1, "reconnect reseeds identical snapshot");
  assert.strictEqual(todos().at(-1).source_op, "hydrate");
  assert.strictEqual(todos().at(-1).observed_at, sourceTime);

  const large = Array.from({ length: 17 }, (_, index) => ({
    name: "🦀".repeat(40), tasks: Array.from({ length: index === 0 ? 101 : 1 }, () => ({
      content: "Task", status: "pending", blocker: "Waiting",
    })).concat([{ content: "Dropped", status: "abandoned", blocker: "No longer needed" }]),
  }));
  await execute("init", large);
  await pause(50);
  const bounded = todos().at(-1);
  assert.strictEqual(bounded.truncated, true);
  assert.deepStrictEqual(bounded.totals, totals(117, 0, 0, 0, 17));
  assert.strictEqual(bounded.phases.length, 16);
  assert.strictEqual(bounded.phases.flatMap((item) => item.tasks).length, 100);
  assert.strictEqual(bounded.phases[0].name, "🦀".repeat(32));
  await execute("init", [{ name: "Unicode", tasks: [{
    content: "🦀".repeat(160), status: "pending", blocker: "🦀".repeat(160),
  }] }]);
  await pause(50);
  assert.strictEqual(todos().at(-1).truncated, true);
  assert.strictEqual(todos().at(-1).phases[0].tasks[0].content, "🦀".repeat(128));
  assert.strictEqual(todos().at(-1).phases[0].tasks[0].blocker, "🦀".repeat(128));
  const escaping = [{ name: "Escaping", tasks: Array.from({ length: 100 }, () => ({
    content: "\u0000".repeat(512), status: "blocked", blocker: "\u0000".repeat(512),
  })) }];
  await execute("init", escaping);
  await pause(50);
  const capped = todos().at(-1);
  assert.strictEqual(capped.truncated, true);
  assert.deepStrictEqual(capped.totals, totals(0, 0, 0, 100));
  assert.ok(Buffer.byteLength(JSON.stringify(capped)) < 64 * 1024);
  assert.ok(capped.phases[0].tasks.length < 100, "escaping drops trailing tasks instead of reporting partial totals");
  await execute("drop", [{ name: "Dropped", tasks: [
    { content: "Dropped task", status: "abandoned" }, { content: "Live", status: "pending" },
  ] }]);
  await pause(50);
  assert.deepStrictEqual(todos().at(-1).phases, [{ name: "Dropped", tasks: [{ content: "Live", status: "pending" }] }]);
  assert.deepStrictEqual(todos().at(-1).totals, totals(1, 0, 0, 0, 1));
  assert.strictEqual(todos().at(-1).truncated, false);
  await execute("drop", [{ name: "Dropped only", tasks: [
    { content: "Dropped task", status: "abandoned" },
  ] }]);
  await pause(50);
  assert.deepStrictEqual(todos().at(-1).phases, [{ name: "Dropped only", tasks: [] }]);
  assert.deepStrictEqual(todos().at(-1).totals, totals(0, 0, 0, 0, 1));
  assert.strictEqual(todos().at(-1).truncated, false);
  await execute("rm", []);
  await pause(50);
  assert.deepStrictEqual(todos().at(-1).phases, []);
  assert.deepStrictEqual(todos().at(-1).totals, totals());
  assert.strictEqual(todos().at(-1).truncated, false);
  await handlers.get("session_shutdown")({}, todoCtx);
}

// An idle model turn does not prove that the native session owns no jobs.
if (process.argv[2]?.includes("st-omp")) {
  let jobs = [{ id: "bg-fixture" }];
  const jobCtx = { ...fullCtx, getAsyncJobSnapshot: () => ({ running: jobs }) };
  await handlers.get("session_start")({}, jobCtx);
  await handlers.get("agent_end")({}, jobCtx);
  await new Promise(resolve => setTimeout(resolve, 250));
  assert.ok(readFrames().some(frame => frame.type === "state" && frame.state === "idle" && frame.backgroundJobs === 1));
  jobs = [];
  await new Promise(resolve => setTimeout(resolve, 1200));
  assert.strictEqual(readFrames().filter(frame => frame.type === "state").at(-1).backgroundJobs, 0);
  const unknownCtx = { ...fullCtx, getAsyncJobSnapshot: () => { throw new Error("unavailable"); } };
  await handlers.get("agent_end")({}, unknownCtx);
  await new Promise(resolve => setTimeout(resolve, 250));
  assert.strictEqual(readFrames().filter(frame => frame.type === "state").at(-1).backgroundJobs, null);
  await handlers.get("session_shutdown")({}, unknownCtx);
}

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
  frames.some(
    (frame) => frame.type === "session" && frame.sessionId === "session-smoke",
  ),
  "session_start must bind the native OMP session before channel readiness",
);
assert.ok(
  frames.some(
    (frame) => frame.type === "ready" && frame.sessionId === "session-smoke",
  ),
  "session_start must acknowledge the bound native OMP session after Rust hello",
);
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
