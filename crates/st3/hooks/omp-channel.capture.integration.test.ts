import assert from "node:assert/strict";
import childProcess from "node:child_process";
import { once } from "node:events";
import extension from "./omp-channel.ts";

// Run with Bun and ST_CAPTURE_TEST_BIN pointing at the candidate st executable.
// A real pipe receiver observes exactly what the extension exports to its channel.
const bin = process.env.ST_CAPTURE_TEST_BIN;
assert.ok(bin, "ST_CAPTURE_TEST_BIN must name the candidate executable");
const registered = "invented-live-registered-credential";
const unregistered = "invented-live-unregistered-credential";
const previousKey = process.env.ANTHROPIC_API_KEY;
process.env.ANTHROPIC_API_KEY = registered;
const receiver = childProcess.spawn(process.execPath, ["-e",
  "process.stdin.pipe(process.stdout)"], { stdio: ["pipe", "pipe", "pipe"] });
let exported = "";
let stderr = "";
receiver.stdout.on("data", (chunk) => { exported += chunk.toString(); });
receiver.stderr.on("data", (chunk) => { stderr += chunk.toString(); });
const globals = globalThis as typeof globalThis & { __stOmpChannel?: unknown };
globals.__stOmpChannel = { bin, child: receiver };
const callbacks = new Map<string, Array<(event: unknown, ctx: unknown) => unknown>>();
const api = {
  on: (name: string, callback: (event: unknown, ctx: unknown) => unknown) => {
    callbacks.set(name, [...(callbacks.get(name) ?? []), callback]);
  },
};
const ctx = { isIdle: () => true, sessionManager: { getSessionId: () => "fixture" } };
const emit = async (name: string, event: unknown) => {
  for (const callback of callbacks.get(name) ?? []) await callback(event, ctx);
};
try {
  extension(api as never);
  for (const credential of [registered, unregistered]) {
    const variants = [credential, JSON.stringify(credential).replace(/i/g, "\\u0069"),
      Buffer.from(credential).toString("base64"), encodeURIComponent(credential),
      credential + "x".repeat(70_000)];
    for (const text of variants) {
      await emit("message_end", { message: { role: "assistant", content: [
        { type: "text", text: text.slice(0, 8) }, { type: "text", text: text.slice(8) },
      ], unknown: credential } });
      await emit("tool_call", { toolName: "fixture", toolCallId: credential,
        input: { nested: { authorization: credential } } });
      await emit("tool_result", { toolCallId: credential, content: text, isError: true });
      // Retrying must never introduce a raw fallback.
      await emit("tool_result", { toolCallId: credential, content: text, isError: true });
    }
  }
  const cyclic: Record<string, unknown> = { message: { role: "assistant" } };
  cyclic.self = cyclic;
  await emit("message_end", cyclic);
  receiver.stdin.end();
  assert.deepEqual(await once(receiver, "exit"), [0, null]);
  assert.equal(stderr, "");
  const frames = exported.trim().split("\n").map((line) => JSON.parse(line))
    .filter((frame) => frame.type === "timeline");
  assert.equal(frames.length, 41);
  assert.ok(frames.every((frame) => frame.policy_version === 1));
  const callIds = new Set(frames.filter((frame) => frame.event === "tool_call")
    .map((frame) => frame.payload.toolCallId));
  assert.equal(callIds.size, 2, "different native calls must not collapse after withholding");
  assert.ok([...callIds].every((id) => typeof id === "string" && id.startsWith("call/")));
  assert.ok(frames.filter((frame) => frame.event === "tool_result" && frame.payload.toolCallId)
    .every((frame) => callIds.has(frame.payload.toolCallId)), "safe generated correlation survives retries");
  for (const credential of [registered, unregistered]) {
    assert.ok(!exported.includes(credential));
    assert.ok(!exported.includes(Buffer.from(credential).toString("base64")));
    assert.ok(!exported.includes(Buffer.from(credential).toString("hex")));
  }
  assert.equal(frames.at(-1).payload.reason, "scanner-failure");
  console.log("PASS fake_credentials_live_extension_pipe_registered_unregistered_encoded_split_oversize_failure_retries");
} finally {
  receiver.kill();
  delete globals.__stOmpChannel;
  if (previousKey === undefined) delete process.env.ANTHROPIC_API_KEY;
  else process.env.ANTHROPIC_API_KEY = previousKey;
}
