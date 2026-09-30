// Runtime smoke of the shipped pi extension: drives the channel-open path far enough that a
// use-before-declaration (TDZ), a broken import, or a top-level throw fails the check — the
// classes a type-only gate is provably blind to. The channel binary is `true`, so the open
// times out its hello and resolves empty; any thrown error fails the smoke.
import assert from "node:assert";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

// A recorder standing in for `st2 driver pi-channel`, so this smoke can assert what the extension
// EMITS and not merely that it loads. With `true` as the channel binary the frames go into a pipe
// nobody reads, which cannot tell a working producer from one that writes nothing at all — and a
// producer that silently writes nothing is indistinguishable from the pre-producer state, where
// every declaration's context reads null. That is the failure this file has to be able to see.
//
// It speaks the two things the extension needs: a protocol-1 hello so `open()` settles without
// waiting out its timeout, and an append of every frame line to a file this smoke reads back.
const dir = fs.mkdtempSync(path.join(os.tmpdir(), "st2-pi-smoke-"));
const framesPath = path.join(dir, "frames.jsonl");
const recorder = path.join(dir, "recorder");
const pidPath = path.join(dir, "channel-pids");
fs.writeFileSync(
  recorder,
  `#!${process.execPath}
import fs from "node:fs";
fs.appendFileSync(${JSON.stringify(pidPath)}, process.pid + "\\n");
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

process.env.ST2_PI_CHANNEL_BIN = recorder;
process.env.ST2_PI_CHANNEL_CATALOG = "/tmp/st2-smoke-catalog";
process.env.ST2_PI_CHANNEL_IDENTITY = "smoke.worker";
process.env.ST2_PI_CHANNEL_RUNTIME_ID = "smoke.worker";
process.env.ST2_PI_CHANNEL_SESSION = "smoke-session";
process.env.ST2_PI_CHANNEL_SEQ = "1";

const mod = await import("./smoke-out/pi-channel.mjs");
assert.strictEqual(typeof mod.default, "function", "extension exports its entry point");

const handlers = new Map();
const pi = {
  on: (name, handler) => handlers.set(name, handler),
};
mod.default(pi);
for (const name of ["session_start", "session_shutdown", "agent_start", "agent_settled"]) {
  assert.ok(handlers.has(name), `extension registers ${name}`);
}

// The harness-context producer must be registered on the events it reads, or it observes nothing.
for (const name of ["message_end", "turn_end", "agent_end", "session_compact"]) {
  assert.ok(handlers.has(name), `extension registers ${name}`);
}

// A ctx with NOTHING the context producer wants. This is the fail-open half and it is the one a
// bare smoke used to cover on its own: an older pi build, or any ctx whose telemetry surface moved,
// must load and deliver mail exactly as before.
const bareCtx = {
  isIdle: () => true,
  ui: { notify: () => {} },
};
// A ctx carrying the surfaces measured on pi 0.84.2 — verbatim shapes from the credential-free lab.
// Without this the context producer's body never executes at all, and a use-before-declaration
// inside it would ship green through both the type gate and the old smoke, which is the exact
// defect class this file exists for.
const fullCtx = {
  ...bareCtx,
  model: { id: "fake-1", provider: "fakelab", contextWindow: 4000 },
  getContextUsage: () => ({ tokens: 23425, contextWindow: 4000, percent: 585.625 }),
  sessionManager: {
    getEntries: () => [{ type: "message" }, { type: "compaction" }, { type: "compaction" }],
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
    getEntries: () => {
      throw new Error("smoke: entries are not readable");
    },
  },
};

// Override one pull without spreading: `throwingCtx`'s `model` is a throwing getter, and a spread
// would fire it here instead of inside the code under test.
const withNulledUsage = (ctx) =>
  Object.create(ctx, {
    getContextUsage: {
      value: () => ({ tokens: null, contextWindow: 4000, percent: null }),
    },
  });

const messageEvent = {
  message: {
    role: "assistant",
    usage: { input: 23300, output: 25, totalTokens: 23425, cost: { total: 0.070305 } },
  },
};

for (const ctx of [bareCtx, fullCtx, throwingCtx]) {
  // Two session starts in a row: the second exercises the predecessor close-and-await path — the
  // exact region the TDZ regression lived in.
  await handlers.get("session_start")({}, ctx);
  await handlers.get("session_start")({}, ctx);
  await handlers.get("agent_start")({}, ctx);
  await handlers.get("message_end")(messageEvent, ctx);
  await handlers.get("turn_end")(messageEvent, ctx);
  await handlers.get("agent_end")({}, ctx);
  // pi withholds tokens and percent here for real; the producer must forward that, not fill it in.
  await handlers.get("session_compact")(
    { reason: "overflow", willRetry: false },
    withNulledUsage(ctx),
  );
  await handlers.get("agent_settled")({}, ctx);
  await handlers.get("session_shutdown")({ reason: "smoke" }, ctx);
}

// A killed channel reconnects without a provider restart or synthetic model turn.
await handlers.get("session_start")({}, bareCtx);
await new Promise((resolve) => setTimeout(resolve, 150));
const oldPid = Number(fs.readFileSync(pidPath, "utf8").trim().split("\n").at(-1));
const beforeIdle = readFrames().filter((frame) => frame.type === "state" && frame.state === "idle").length;
process.kill(oldPid, "SIGKILL");
await new Promise((resolve) => setTimeout(resolve, 1000));
assert.notStrictEqual(Number(fs.readFileSync(pidPath, "utf8").trim().split("\n").at(-1)), oldPid);
assert.ok(readFrames().filter((frame) => frame.type === "state" && frame.state === "idle").length > beforeIdle);
await handlers.get("session_shutdown")({ reason: "quit" }, bareCtx);

// Give the recorder a moment to drain what was written to its stdin, then assert the wire.
await new Promise((resolve) => setTimeout(resolve, 500));
const frames = readFrames();
const context = frames.filter((frame) => frame.type === "context");
assert.ok(context.length > 0, "the producer must emit context frames, not merely load");

// The exact keys `src/pi_channel.rs::context_frame` decodes. Both halves of this contract live in
// different languages and different files, so nothing but this assertion couples them: flatten the
// reading or rename a key and every Rust fixture still passes while the record is never written.
const reading = context.find((frame) => frame.reading);
assert.ok(reading, "a context frame must carry a `reading` object");
// Every leg is always present, `null` where withheld — one absence convention on the wire, so the
// Rust decoder never has to tell "absent" from "the harness said it does not know".
for (const key of ["usedTokens", "windowTokens", "usedPercent", "model", "costUsd"]) {
  assert.ok(key in reading.reading, `reading carries ${key}`);
}

// Selected by predicate, not by position: the contexts are driven in a fixed order but which one
// produced a given frame is not what is under test, and asserting on `frames[0]` would make this
// break whenever the loop gains a case.
const known = context.find((frame) => typeof frame.reading?.usedTokens === "number");
assert.ok(known, "a populated context must produce a reading with real numbers");
assert.strictEqual(known.reading.usedTokens, 23425, "pi's numerator is totalTokens");
assert.strictEqual(known.reading.windowTokens, 4000);
assert.strictEqual(known.reading.usedPercent, 585.625, "carried raw, never clamped");
assert.strictEqual(known.reading.model, "fake-1");

// The cost hold, on the wire. `session_start` legitimately carries `costUsd: null` — the hold is
// cleared on session replacement, so a fresh session does not restate its predecessor's cost — and
// a later frame from an event carrying no cost of its own must still restate the held one, or the
// record's wholesale field replacement would erase it.
const withCost = context.find((frame) => typeof frame.reading?.costUsd === "number");
assert.ok(withCost, "the held cost must be restated on frames after a message-bearing event");
assert.strictEqual(withCost.reading.costUsd, 0.070305);

const edges = context.filter((frame) => frame.compaction);
assert.ok(edges.length > 0, "a compaction edge must ride a context frame");
for (const edge of edges) {
  assert.strictEqual(edge.compaction.trigger, "overflow", "pi names its own trigger");
}
// A context whose session store cannot be read still sends the edge, with no count: st2 then
// counts it itself. The edge is never what gets dropped.
assert.ok(
  edges.some((edge) => edge.compaction.count === null),
  "an unreadable session store must still send the edge, countless",
);
const durable = edges.find((edge) => typeof edge.compaction.count === "number");
assert.ok(durable, "a readable session store must supply the durable count");
assert.strictEqual(durable.compaction.count, 2, "the count is getEntries() filtered to compactions");
// The pairing the write guard makes load-bearing: the withheld reading must be in the SAME frame
// as the edge, or it lands in no write until the heartbeat comes due.
assert.strictEqual(durable.reading.usedTokens, null, "the withheld reading rides the edge");
assert.strictEqual(durable.reading.usedPercent, null);
assert.strictEqual(durable.reading.windowTokens, 4000, "pi still knows its denominator");

fs.rmSync(dir, { recursive: true, force: true });
console.log("pi extension smoke: ok");
process.exit(0);
