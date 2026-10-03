// Companion to the shipped omp-channel.ts, run only in the admission scratch RPC session.
// Record real harness events; never manufacture lifecycle or approval evidence.
import fs from "node:fs";

export default function admissionProbe(pi: any) {
  const record = (value: unknown) => fs.appendFileSync(
    process.env.ST_ADMISSION_TRACE!, JSON.stringify(value) + "\n",
  );
  record({ type: "extension_load", sendUserMessage: typeof pi.sendUserMessage,
    sendMessage: typeof pi.sendMessage, on: typeof pi.on });
  pi.registerTool({
    name: "admission_fixture", label: "Admission", description: "Harmless empty admission fixture",
    parameters: { type: "object", properties: {}, required: [] },
    execute: async () => ({ content: [{ type: "text", text: "fixture done" }] }),
  });
  for (const name of ["session_start", "agent_start", "turn_start", "message_start",
    "message_end", "turn_end", "agent_end", "tool_approval_requested", "tool_approval_resolved"]) {
    pi.on(name, (event: any, ctx: any) => {
      record({ type: name, event, idle: ctx.isIdle() });
      if (name === "agent_end" && event.willContinue !== true) {
        let samples = 0;
        const timer = setInterval(() => {
          const idle = ctx.isIdle();
          record({ type: "idle_sample", idle });
          if (idle || ++samples >= 80) clearInterval(timer);
        }, 25);
      }
    });
  }
}
