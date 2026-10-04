// Native control admission is not atomic model+effort application and is not durable deduplication.
// A void ExtensionAPI sendMessage return proves nothing: only the exact native message event settles input.
import { createHash, randomUUID } from "node:crypto";
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
// OMP's native thinking selector includes max, unlike older pi-family declarations.
type NativeControlAPI = Omit<ExtensionAPI, "setThinkingLevel"> & {
  setThinkingLevel(level: "off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"): void;
};
type NativeControlContext = ExtensionContext & {
  models?: { current(): ExtensionContext["model"] };
};

export type ControlBinding = { desired_revision: string; incarnation_id: string; session_id: string; turn_id: string | null };
type Input = { type: "input"; operation_id: string; entry_id: string; actor: string; content: string; lane: "steer" | "follow_up"; binding: ControlBinding };
type SetModel = { type: "set_model"; operation_id: string; binding: ControlBinding; model_revision: string; provider: string; model_id: string; effort?: string };
type Command = Input | SetModel;
type Receipt = { type: "harness_control_receipt"; operation_id: string; binding: ControlBinding; status: "applied" | "rejected" | "indeterminate"; reason?: string; result?: Record<string, unknown> };
type ModelDescriptor = {
  revision: string; available: boolean; complete: boolean; source: "native-extension-model-registry";
  choices: { provider: string; id: string; reasoning: boolean; supported_efforts: string[] }[];
  selected: { provider: string; id: string; effective_effort: string | null; configured_effort: "unknown" } | null;
  atomic_model_effort: false;
};
const record = (value: unknown): Record<string, unknown> | undefined => value !== null && typeof value === "object" && !Array.isArray(value) ? value as Record<string, unknown> : undefined;
const bindingOf = (value: unknown): ControlBinding | undefined => {
  const raw = record(value);
  if (!raw || typeof raw.desired_revision !== "string" || typeof raw.incarnation_id !== "string" || typeof raw.session_id !== "string" || !(raw.turn_id === null || typeof raw.turn_id === "string")) return;
  return { desired_revision: raw.desired_revision, incarnation_id: raw.incarnation_id, session_id: raw.session_id, turn_id: raw.turn_id };
};
const sameBinding = (a: ControlBinding, b: ControlBinding) => a.desired_revision === b.desired_revision && a.incarnation_id === b.incarnation_id && a.session_id === b.session_id && a.turn_id === b.turn_id;
const efforts = (model: unknown): string[] => {
  const raw = record(model);
  const choices = record(raw?.thinking)?.efforts;
  if (raw?.reasoning !== true) return [];
  // Native model-controls offers off in addition to the model's declared efforts.
  return ["off", ...(Array.isArray(choices) ? choices.filter((choice): choice is string => typeof choice === "string" && ["minimal", "low", "medium", "high", "xhigh", "max"].includes(choice)) : [])];
};

export const createHarnessControl = (pi: NativeControlAPI, send: (frame: Record<string, unknown>) => void): { handle: (frame: unknown, ctx: ExtensionContext) => boolean; observe: () => void; replay: () => void } => {
  let context: ExtensionContext | undefined;
  let desired: ControlBinding | undefined;
  let turnId: string | null = null;
  let transitioning = false;
  let mutation: string | undefined;
  const pending = new Map<string, Command>();
  const receipts = new Map<string, Receipt>();
  const current = (): ControlBinding | undefined => context && desired ? { ...desired, session_id: context.sessionManager.getSessionId(), turn_id: turnId } : undefined;
  const idle = () => { try { return context?.isIdle() === true; } catch { return false; } };
  const selectedModel = (): ExtensionContext["model"] | undefined => {
    const models = (context as NativeControlContext | undefined)?.models;
    return typeof models?.current === "function" ? models.current() : undefined;
  };
  const modelDescriptor = (): ModelDescriptor => {
    let values: Omit<ModelDescriptor, "revision">;
    try {
      const model = selectedModel();
      values = {
        available: typeof (context as NativeControlContext | undefined)?.models?.current === "function", complete: context !== undefined, source: "native-extension-model-registry", atomic_model_effort: false,
        choices: (context?.modelRegistry.getAvailable() ?? []).map(item => ({ provider: item.provider, id: item.id, reasoning: item.reasoning, supported_efforts: efforts(item) })),
        selected: model ? { provider: model.provider, id: model.id, effective_effort: pi.getThinkingLevel() ?? null, configured_effort: "unknown" } : null,
      };
    } catch {
      values = { available: false, complete: false, source: "native-extension-model-registry", choices: [], selected: null, atomic_model_effort: false };
    }
    return { ...values, revision: createHash("sha256").update(JSON.stringify(values)).digest("hex") };
  };
  const snapshot = () => {
    if (!context) return;
    send({ type: "harness_control_state", session_id: context.sessionManager.getSessionId(), turn_id: turnId, idle: idle(), input_supported: !transitioning && !!current(), reason: transitioning ? "session-transition-state-unknown" : undefined,
      models: modelDescriptor(),
      approval: { supported: false, reason: "native-live-approval-api-unavailable" } });
  };
  const settle = (command: Command, status: Receipt["status"], reason?: string, result?: Record<string, unknown>) => {
    if (receipts.has(command.operation_id)) return;
    const receipt: Receipt = { type: "harness_control_receipt", operation_id: command.operation_id, binding: command.binding, status, ...(reason ? { reason } : {}), ...(result ? { result } : {}) };
    pending.delete(command.operation_id);
    receipts.set(command.operation_id, receipt);
    send(receipt);
  };
  const invalidate = (reason: string) => {
    for (const command of pending.values()) settle(command, "indeterminate", reason);
  };
  // Use native cancellable lifecycle hooks, not a timer or keystroke fallback. No timeout may
  // release admission: a queued async native submission could still mutate its destination later.
  for (const name of ["session_before_switch", "session_before_branch", "session_before_tree"] as const) {
    pi.on(name, (_event, ctx) => {
      if ((ctx as ExtensionContext & { agent?: { kind?: string } }).agent?.kind === "sub") return;
      if (pending.size || mutation) return { cancel: true };
      transitioning = true;
      snapshot();
    });
  }
  for (const name of ["session_start", "session_switch", "session_branch", "session_tree"] as const) {
    pi.on(name, (_event, ctx) => {
      if ((ctx as ExtensionContext & { agent?: { kind?: string } }).agent?.kind === "sub") return;
      invalidate("native-session-or-branch-replaced");
      context = ctx;
      turnId = null;
      transitioning = false;
      snapshot();
    });
  }
  pi.on("agent_start", (_event, ctx) => {
    if ((ctx as ExtensionContext & { agent?: { kind?: string } }).agent?.kind === "sub") return;
    context = ctx;
    turnId = randomUUID();
    snapshot();
  });
  pi.on("agent_end", (_event, ctx) => {
    if ((ctx as ExtensionContext & { agent?: { kind?: string } }).agent?.kind === "sub") return;
    context = ctx;
    snapshot();
  });
  for (const name of ["message_start", "message_end"] as const) {
    pi.on(name, (event, ctx) => {
      if ((ctx as ExtensionContext & { agent?: { kind?: string } }).agent?.kind === "sub") return;
      const message = record(event.message);
      const details = record(message?.details);
      if (message?.role !== "custom" || message.customType !== "st-control-input" || typeof details?.operation_id !== "string") return;
      const command = pending.get(details.operation_id);
      if (!command || command.type !== "input" || details.actor !== command.actor || details.entry_id !== command.entry_id) return;
      const live = current();
      if (ctx.sessionManager.getSessionId() !== command.binding.session_id || transitioning || live?.incarnation_id !== command.binding.incarnation_id || live.desired_revision !== command.binding.desired_revision) settle(command, "indeterminate", "native-binding-replaced");
      else settle(command, "applied", undefined, { native_event: name, turn_id: turnId });
      snapshot();
    });
  }
  pi.on("session_shutdown", () => { transitioning = true; invalidate("native-process-shutdown"); });

  const setModel = async (command: SetModel, ctx: ExtensionContext) => {
    mutation = command.operation_id;
    try {
      const before = modelDescriptor();
      if (!before.available) { settle(command, "rejected", "native-model-registry-unavailable"); return; }
      if (before.revision !== command.model_revision) { settle(command, "rejected", "stale-native-model-revision"); return; }
      const model = ctx.modelRegistry.getAvailable().find(item => item.provider === command.provider && item.id === command.model_id);
      if (!model) { settle(command, "rejected", "model-unavailable"); return; }
      const requested = command.effort;
      if (requested !== undefined && !efforts(model).includes(requested)) { settle(command, "rejected", "effort-unsupported"); return; }
      const accepted = await pi.setModel(model);
      const binding = current();
      if (!binding || !sameBinding(binding, command.binding) || transitioning) { settle(command, "indeterminate", "native-binding-replaced"); return; }
      if (!accepted) { settle(command, "rejected", "model-credential-unavailable"); return; }
      const after = modelDescriptor();
      if (!after.available || JSON.stringify(before.choices) !== JSON.stringify(after.choices)) { settle(command, "indeterminate", "native-model-catalog-replaced"); return; }
      if (requested !== undefined) {
        // The public setter's choices are narrower than model metadata. Reject unknown choices
        // before mutation; no cast converts a provider-only effort into an extension capability.
        switch (requested) {
          case "off": case "minimal": case "low": case "medium": case "high": case "xhigh": case "max": pi.setThinkingLevel(requested); break;
          default: settle(command, "indeterminate", "model-applied-effort-api-unavailable", { effective_effort: pi.getThinkingLevel() ?? null }); return;
        }
      }
      const observed = selectedModel();
      const effective = pi.getThinkingLevel() ?? null;
      if (observed?.provider !== command.provider || observed.id !== command.model_id || (requested !== undefined && effective !== requested)) settle(command, "indeterminate", "native-effective-value-mismatch", { provider: observed?.provider, id: observed?.id, effective_effort: effective });
      else settle(command, "applied", undefined, { provider: observed.provider, id: observed.id, effective_effort: effective, atomic_model_effort: false });
    } catch { settle(command, "indeterminate", "native-model-operation-failed"); }
    finally { mutation = undefined; snapshot(); }
  };
  const handle = (frame: unknown, ctx: ExtensionContext): boolean => {
    const raw = record(frame);
    if (raw?.type === "harness_control_binding") {
      desired = bindingOf(raw.binding);
      context = ctx;
      snapshot();
      for (const receipt of receipts.values()) send(receipt);
      return true;
    }
    if (raw?.type !== "harness_control") return false;
    context = ctx;
    const wire = record(raw.command);
    const binding = bindingOf(wire?.binding);
    if (!wire || typeof wire.operation_id !== "string" || !binding) return true;
    const operationId = wire.operation_id;
    const existing = receipts.get(operationId);
    if (existing) { send(existing); return true; }
    if (pending.has(operationId)) return true;
    const reject = (reason: string) => {
      const receipt: Receipt = { type: "harness_control_receipt", operation_id: operationId, binding, status: "rejected", reason };
      receipts.set(operationId, receipt);
      send(receipt);
    };
    const live = current();
    if (!live || !sameBinding(binding, live)) { reject("stale-native-binding"); return true; }
    if (transitioning || mutation || pending.size) { reject(transitioning ? "session-transition-state-unknown" : "native-control-busy"); return true; }
    if (wire.type === "input" && typeof wire.entry_id === "string" && typeof wire.actor === "string" && typeof wire.content === "string" && (wire.lane === "steer" || wire.lane === "follow_up")) {
      const command: Input = { type: "input", operation_id: operationId, binding, entry_id: wire.entry_id, actor: wire.actor, content: wire.content, lane: wire.lane };
      pending.set(operationId, command);
      try {
        pi.sendMessage({ customType: "st-control-input", content: command.content, display: true, details: { operation_id: operationId, actor: command.actor, entry_id: command.entry_id }, attribution: "user" }, { triggerTurn: true, deliverAs: command.lane === "follow_up" ? "followUp" : "steer" });
      } catch { settle(command, "indeterminate", "native-input-invocation-failed"); }
    } else if (wire.type === "set_model" && typeof wire.provider === "string" && typeof wire.model_id === "string" && typeof wire.model_revision === "string" && (wire.effort === undefined || typeof wire.effort === "string")) {
      const command: SetModel = { type: "set_model", operation_id: operationId, binding, model_revision: wire.model_revision, provider: wire.provider, model_id: wire.model_id, ...(typeof wire.effort === "string" ? { effort: wire.effort } : {}) };
      if (command.effort !== undefined && !["off", "minimal", "low", "medium", "high", "xhigh", "max"].includes(command.effort)) { reject("effort-api-unsupported"); return true; }
      pending.set(operationId, command);
      void setModel(command, ctx);
    } else reject("command-unsupported");
    return true;
  };
  return { handle, observe: snapshot, replay: () => { snapshot(); for (const receipt of receipts.values()) send(receipt); } };
};
