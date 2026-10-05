//! Public native lifecycle and terminal-input APIs only. No dialog internals or queued chat.
import { randomUUID } from "node:crypto";
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import type { ControlBinding } from "./omp-harness-control.ts";

type Option = { label: string; description?: string; preview?: string };
type Question = { id: string; question: string; options: Option[]; multi?: boolean; recommended?: number };
type Answer = { id: string; selected_options: string[]; custom_input?: string };
type Ask = { tool_call_id: string; questions: Question[] };
type Command = { type: "answer_ask"; operation_id: string; binding: ControlBinding; tool_call_id: string; answers: Answer[] };
type Step = { data: string; question_index: number; surface: "question" | "custom" | "review" };
const record = (value: unknown): Record<string, unknown> | undefined => value !== null && typeof value === "object" && !Array.isArray(value) ? value as Record<string, unknown> : undefined;
const sameBinding = (a: ControlBinding, b: ControlBinding) => a.desired_revision === b.desired_revision && a.incarnation_id === b.incarnation_id && a.session_id === b.session_id && a.turn_id === b.turn_id;
const parseQuestions = (input: unknown): Question[] | undefined => {
  const values = record(input)?.questions;
  if (!Array.isArray(values) || !values.length) return;
  const questions: Question[] = [];
  for (const value of values) {
    const q = record(value);
    if (!q || typeof q.id !== "string" || !q.id || typeof q.question !== "string" || !Array.isArray(q.options) || (q.multi !== undefined && typeof q.multi !== "boolean") || (q.recommended !== undefined && (!Number.isInteger(q.recommended) || Number(q.recommended) < 0))) return;
    const options: Option[] = [];
    for (const value of q.options) {
      const option = record(value);
      if (!option || typeof option.label !== "string" || (option.description !== undefined && typeof option.description !== "string") || (option.preview !== undefined && typeof option.preview !== "string")) return;
      options.push({ label: option.label, ...(typeof option.description === "string" ? { description: option.description } : {}), ...(typeof option.preview === "string" ? { preview: option.preview } : {}) });
    }
    if (questions.some(prior => prior.id === q.id)) return;
    questions.push({ id: q.id, question: q.question, options, ...(typeof q.multi === "boolean" ? { multi: q.multi } : {}), ...(typeof q.recommended === "number" ? { recommended: q.recommended } : {}) });
  }
  return questions;
};
const parseAnswers = (input: unknown): Answer[] | undefined => {
  if (!Array.isArray(input)) return;
  const answers: Answer[] = [];
  for (const value of input) {
    const a = record(value);
    if (!a || typeof a.id !== "string" || !Array.isArray(a.selected_options) || !a.selected_options.every((label): label is string => typeof label === "string") || (a.custom_input !== undefined && typeof a.custom_input !== "string")) return;
    answers.push({ id: a.id, selected_options: a.selected_options, ...(typeof a.custom_input === "string" ? { custom_input: a.custom_input } : {}) });
  }
  return answers;
};
const answersEqual = (a: Answer[], b: Answer[]) => a.length === b.length && a.every((answer, index) => { const other = b[index]; return other?.id === answer.id && other.custom_input === answer.custom_input && answer.selected_options.length === other.selected_options.length && answer.selected_options.every(label => other.selected_options.includes(label)); });

export const createHarnessAsk = (pi: ExtensionAPI, send: (frame: Record<string, unknown>) => void, current: () => ControlBinding | undefined, observe: () => void) => {
  let ask: Ask | undefined;
  let unsafe: string | undefined;
  let unsubscribe: (() => void) | undefined;
  let guarded: Command | undefined;
  let stepToken: string | undefined;
  let steps: Step[] = [];
  let stepIndex = 0;
  const activeCalls = new Set<string>();
  const receipts = new Map<string, Record<string, unknown>>();
  const settle = (command: Command, status: "applied" | "rejected" | "indeterminate", reason?: string, answers?: Answer[]) => {
    const receipt = { type: "harness_control_receipt", operation_id: command.operation_id, binding: command.binding, status, ...(reason ? { reason } : {}), ...(answers ? { result: { native_event: "tool_result", tool_call_id: command.tool_call_id, answers } } : {}) };
    guarded = undefined; stepToken = undefined; steps = [];
    receipts.set(command.operation_id, receipt); send(receipt);
  };
  const requestStep = () => {
    const command = guarded; const step = steps[stepIndex]; const live = current();
    if (!command || !step || !ask || ask.tool_call_id !== command.tool_call_id) return;
    if (!live || !sameBinding(command.binding, live)) { settle(command, "indeterminate", "native-binding-replaced"); return; }
    stepToken = `st-ask-${randomUUID()}`;
    send({ type: "harness_ask_terminal_input", operation_id: command.operation_id, binding: command.binding, tool_call_id: command.tool_call_id, token: stepToken, question_index: step.question_index, surface: step.surface });
  };
  const install = (ctx: ExtensionContext) => {
    if (unsubscribe) return;
    unsubscribe = ctx.ui.onTerminalInput(data => {
      const match = /^\x1b\[200~(st-ask-[a-zA-Z0-9-]+)\x1b\[201~$/.exec(data);
      if (match) {
        const token = match[1];
        const live = current(); const command = guarded; const step = steps[stepIndex];
        // The reserved protocol prefix is consumed even after settlement, session reset,
        // or process replacement. A late/foreign token can never become chat input.
        if (!command || !step || token !== stepToken || !live || !sameBinding(command.binding, live) || ask?.tool_call_id !== command.tool_call_id) return { consume: true };
        stepToken = undefined; stepIndex += 1;
        // Microtask runs after the native focused component handles this transformed
        // event; the owner then checks the next actual screen before another token.
        queueMicrotask(requestStep);
        return { data: step.data };
      }
      if (guarded) return { consume: true };
      if (ask) { unsafe = "native-ask-edited-in-terminal"; observe(); }
      return undefined;
    });
  };
  const reset = (reason: string) => {
    if (guarded) settle(guarded, "indeterminate", reason);
    ask = undefined; unsafe = undefined; activeCalls.clear(); observe();
  };
  pi.on("tool_execution_start", (event, ctx) => {
    if (event.toolName !== "ask" || (ctx as ExtensionContext & { agent?: { kind?: string } }).agent?.kind === "sub") return;
    activeCalls.add(event.toolCallId);
    if (activeCalls.size !== 1) {
      unsafe = "ambiguous-native-ask";
      if (guarded) settle(guarded, "indeterminate", unsafe);
      observe(); return;
    }
    const questions = parseQuestions(event.args);
    if (!questions) return;
    ask = { tool_call_id: event.toolCallId, questions };
    const terminal = process.stdin.isTTY === true && process.stdout.isTTY === true && !process.argv.some((argument, index) => argument.startsWith("--mode=rpc") || (argument === "--mode" && process.argv[index + 1]?.startsWith("rpc")));
    unsafe = terminal && typeof ctx.ui.askDialog === "function" && ctx.hasUI ? undefined : "native-rich-ask-terminal-unavailable";
    if (questions.some(q => new Set(q.options.map(option => option.label)).size !== q.options.length)) unsafe = "ambiguous-native-ask-options";
    try { install(ctx); } catch { unsafe = "native-terminal-input-guard-unavailable"; }
    observe();
  });
  pi.on("tool_result", (event, ctx) => {
    if (event.toolName !== "ask" || (ctx as ExtensionContext & { agent?: { kind?: string } }).agent?.kind === "sub") return;
    activeCalls.delete(event.toolCallId);
    if (ask?.tool_call_id !== event.toolCallId) return;
    if (guarded) {
      const details = record(event.details);
      const results = ask.questions.length === 1 ? [details] : details?.results;
      const answers: Answer[] = [];
      if (Array.isArray(results)) for (let index = 0; index < results.length; index += 1) {
        const result = record(results[index]); const question = ask.questions[index];
        if (!result || !question || (ask.questions.length > 1 && result.id !== question.id) || !Array.isArray(result.selectedOptions) || !result.selectedOptions.every((label): label is string => typeof label === "string") || (result.customInput !== undefined && typeof result.customInput !== "string")) break;
        answers.push({ id: question.id, selected_options: result.selectedOptions, ...(typeof result.customInput === "string" ? { custom_input: result.customInput } : {}) });
      }
      const live = current();
      if (!event.isError && live && sameBinding(live, guarded.binding) && answersEqual(answers, guarded.answers)) {
        // Receipt serializes the actual answers, in caller order only for the unordered multi set.
        const actual = answers.map((answer, index) => ({ ...answer, selected_options: guarded?.answers[index]?.selected_options.filter(label => answer.selected_options.includes(label)) ?? answer.selected_options }));
        settle(guarded, "applied", undefined, actual);
      } else settle(guarded, "indeterminate", "native-ask-result-mismatch");
    }
    ask = undefined; unsafe = undefined; observe();
  });
  for (const name of ["session_start", "session_switch", "session_branch", "session_tree"] as const) pi.on(name, () => reset("native-session-replaced"));
  pi.on("session_shutdown", () => reset("native-process-shutdown"));
  return {
    state: () => ({ pending_ask: ask ?? null, ask_supported: !!ask && !unsafe && !guarded, ask_reason: unsafe ?? (guarded ? "native-ask-answer-in-flight" : ask ? null : "no-pending-native-ask") }),
    busy: () => !!guarded,
    replay: () => { for (const receipt of receipts.values()) send(receipt); },
    handle: (wire: Record<string, unknown>, binding: ControlBinding): boolean => {
      if (wire.type === "harness_ask_terminal_failure") {
        if (guarded?.operation_id === wire.operation_id && guarded.tool_call_id === wire.tool_call_id) { unsafe = "native-ask-terminal-delivery-failed"; settle(guarded, "indeterminate", "native-ask-terminal-delivery-failed"); observe(); }
        return true;
      }
      if (wire.type !== "answer_ask" || typeof wire.operation_id !== "string") return false;
      const existing = receipts.get(wire.operation_id); if (existing) { send(existing); return true; }
      if (guarded?.operation_id === wire.operation_id) return true;
      const answers = parseAnswers(wire.answers);
      const command: Command = { type: "answer_ask", operation_id: wire.operation_id, binding, tool_call_id: typeof wire.tool_call_id === "string" ? wire.tool_call_id : "", answers: answers ?? [] };
      const reject = (reason: string) => { const receipt = { type: "harness_control_receipt", operation_id: command.operation_id, binding, status: "rejected", reason }; receipts.set(command.operation_id, receipt); send(receipt); };
      if (!ask || ask.tool_call_id !== command.tool_call_id || unsafe || guarded || activeCalls.size !== 1) { reject(unsafe ?? "stale-or-busy-native-ask"); return true; }
      if (!answers || answers.length !== ask.questions.length || answers.some((answer, index) => { const q = ask?.questions[index]; return !q || answer.id !== q.id || new Set(answer.selected_options).size !== answer.selected_options.length || answer.selected_options.some(label => !q.options.some(option => option.label === label)) || (!q.multi && answer.selected_options.length + Number(answer.custom_input !== undefined) !== 1) || (answer.custom_input !== undefined && (!answer.custom_input.trim() || /[\x00-\x08\x0b-\x1f\x7f]/.test(answer.custom_input))); })) { reject("invalid-native-ask-answers"); return true; }
      steps = [];
      for (let index = 0; index < ask.questions.length; index += 1) {
        const q = ask.questions[index]!; const answer = answers[index]!;
        const key = (data: string, surface: Step["surface"] = "question") => steps.push({ data, question_index: index, surface });
        const move = (target: number) => { for (let i = 0; i <= q.options.length; i += 1) key("\x1b[A"); for (let i = 0; i < target; i += 1) key("\x1b[B"); };
        for (const label of answer.selected_options) { move(q.options.findIndex(option => option.label === label)); key(q.multi ? " " : "\r"); }
        if (answer.custom_input !== undefined) { move(q.options.length); key("\r"); key(`\x1b[200~${answer.custom_input}\x1b[201~`, "custom"); key("\r", "custom"); }
        else if (q.multi) { move(0); key("\r"); }
      }
      // Ordinary one-question Enter submits directly; custom multi jumps to Review.
      if (ask.questions.length > 1 || (ask.questions[0]?.multi && answers[0]?.custom_input !== undefined)) steps.push({ data: "\r", question_index: ask.questions.length - 1, surface: "review" });
      guarded = command; stepIndex = 0; observe(); requestStep();
      return true;
    },
  };
};
