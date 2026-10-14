/**
 * A native harness prompt as web, tui and ios show it: which of the answers the daemon offers a
 * client may show, and the form they read as, worded and numbered like the harness's own.
 *
 * The Rust twin is `crates/st3-ui-model/src/prompts.rs` (stui). Both are checked against
 * `tests/fixtures/prompt-forms.json`, so web, tui and ios offer the same answers with the same
 * words. Only answers the daemon lists are ever shown, and what an answer means is the daemon's:
 * a label says it in the harness's words and never more. Choices the harness's dialog has and st
 * cannot send stay in its terminal, and the form says so.
 */

/** A tool call the seat's harness recorded and has no result for yet. */
export type PromptCall = { tool: string; input: unknown };
export type PromptHarness = 'claude' | 'codex' | 'other';
export type PromptKind = 'permission' | 'plan' | 'question' | 'mcp' | 'trust' | 'denial' | 'other';

/** What a prompt asks, read from the answers the daemon lists for it. */
export function promptKind(answers: readonly string[]): PromptKind {
  const has = (answer: string) => answers.includes(answer);
  if (has('answer')) return 'question';
  if (has('approve')) return 'plan';
  if (has('approve-once') || has('approve-kind')) return 'denial';
  if (has('use') || has('use-all') || has('skip')) return 'mcp';
  if (has('trust')) return 'trust';
  if (has('allow')) return 'permission';
  return 'other';
}

/** A call's input as lines a person reads, each field on its own line, further lines indented. */
export function promptCallLines(input: unknown): string[] {
  if (input === null || input === undefined) return [];
  if (typeof input === 'string') return input.split('\n');
  if (typeof input !== 'object' || Array.isArray(input)) return JSON.stringify(input, null, 2).split('\n');
  const lines: string[] = [];
  for (const [key, field] of Object.entries(input as Record<string, unknown>)) {
    const text = typeof field === 'string' ? field : JSON.stringify(field, null, 2);
    const [first = '', ...rest] = text.split('\n');
    lines.push(`${key}: ${first}`, ...rest.map(line => `  ${line}`));
  }
  return lines;
}

function hasQuestions(call: PromptCall): boolean {
  if (call.tool !== 'AskUserQuestion') return false;
  const asked = (call.input as { questions?: unknown } | null)?.questions;
  if (!Array.isArray(asked) || !asked.length) return false;
  return asked.every(each => {
    const question = each as { question?: unknown; options?: unknown };
    return typeof question.question === 'string' && Array.isArray(question.options) && question.options.length > 0
      && question.options.every(option => typeof (option as { label?: unknown }).label === 'string' && !!(option as { label: string }).label);
  });
}

/**
 * The answers a client may offer: `allow` and `approve` only beside exactly one call that can be
 * shown; `answer` only when that call's questions can be chosen from; every other answer always.
 */
export function offeredPromptAnswers(answers: readonly string[], calls: readonly PromptCall[]): string[] {
  const certain = calls.length === 1 && promptCallLines(calls[0].input).length > 0;
  return answers.filter(answer => {
    if (answer === 'allow' || answer === 'approve') return certain;
    if (answer === 'answer') return calls.length === 1 && hasQuestions(calls[0]);
    return true;
  });
}

/** Enter may take it when it is marked: what is shown in full, or a safe way past. */
export function enterAnswers(answer: string): boolean {
  return answer === 'allow' || answer === 'approve' || answer === 'answer' || answer === 'skip';
}

/** It refuses or ends something: never marked first, never under Enter. */
export function refuses(answer: string): boolean {
  return answer === 'deny' || answer === 'exit' || answer === 'quit';
}

const READS = new Set(['Read', 'Glob', 'Grep', 'LS', 'NotebookRead']);

function fileName(input: unknown): string | null {
  const fields = (input ?? {}) as Record<string, unknown>;
  const path = [fields.file_path, fields.notebook_path, fields.path].find((value): value is string => typeof value === 'string');
  return path?.split('/').filter(Boolean).pop() ?? null;
}

function questionLine(harness: PromptHarness, kind: PromptKind, prompt: string, call: PromptCall | null): string {
  const first = prompt.split('\n').map(line => line.trim().replace(/^#+/, '').trim()).find(Boolean) ?? '';
  const tool = call?.tool ?? '';
  if (harness === 'claude' && kind === 'permission') {
    if (READS.has(tool)) return 'Allow this read outside the working directories?';
    if (tool === 'Edit' || tool === 'MultiEdit' || tool === 'NotebookEdit') {
      const name = call && fileName(call.input);
      return name ? `Do you want to make this edit to ${name}?` : 'Do you want to make this edit?';
    }
    if (tool === 'Write') {
      const name = call && fileName(call.input);
      return name ? `Do you want to create ${name}?` : 'Do you want to create this file?';
    }
    if (tool === 'WebFetch') return 'Do you want to allow Claude to fetch this content?';
    return 'Do you want to proceed?';
  }
  if (harness === 'codex' && kind === 'permission') {
    const lower = tool.toLowerCase();
    if (lower.includes('patch') || lower.includes('file') || lower.includes('edit')) return 'Would you like to make the following edits?';
    if (lower.includes('permission')) return 'Would you like to grant these permissions?';
    return 'Would you like to run the following command?';
  }
  if (kind === 'plan') return 'Would you like to proceed?';
  if (kind === 'question') return 'Ready to submit your answers?';
  if (kind === 'denial') return "Claude's auto mode denied this call. Allow it?";
  if (kind === 'permission') return 'Do you want to proceed?';
  return first;
}

/** The harness's words for an answer st sends on a prompt of `kind`. */
export function promptAnswerLabel(harness: PromptHarness, kind: PromptKind, tool: string | null, answer: string): string {
  const read = tool !== null && READS.has(tool);
  if (kind === 'permission') {
    if (harness === 'claude' && read && answer === 'allow') return 'Yes, but ask again next time';
    if (harness === 'claude' && read && answer === 'deny') return 'No, and ask again next time';
    if (harness === 'codex' && answer === 'allow') return 'Yes, proceed';
    if (answer === 'allow') return 'Yes';
    if (answer === 'deny') return 'No';
  }
  if (kind === 'plan' && answer === 'approve') return 'Yes';
  if (kind === 'plan' && answer === 'deny') return 'No, keep planning';
  if (kind === 'question' && answer === 'answer') return 'Submit answers';
  if (kind === 'question' && answer === 'deny') return 'No, decline to answer';
  switch (answer) {
    case 'use': return 'Use this MCP server';
    case 'use-all': return 'Use this and all future MCP servers in this project';
    case 'skip': return 'Continue without using this MCP server';
    case 'trust': return harness === 'codex' ? 'Yes, continue' : 'Yes, I trust this folder';
    case 'exit': return 'No, exit';
    case 'quit': return 'No, quit';
    case 'approve-once': return 'Yes, allow it once';
    case 'approve-kind': return 'Yes, and allow calls of its kind for this seat';
    case 'dismiss': return 'No, leave it denied';
    case 'allow': return 'Yes';
    case 'deny': return 'No';
    default: return answer;
  }
}

export type PromptFormOption = { answer: string; label: string; enter: boolean; refuses: boolean };
export type PromptForm = { kind: PromptKind; question: string; options: PromptFormOption[]; mark: number | null; elsewhere: string | null };

/**
 * The form for a prompt: its question line and options numbered from 1 in the harness's order,
 * refusing last, with the option Enter would take marked (else the first that refuses nothing).
 */
export function promptForm(harness: PromptHarness, answers: readonly string[], prompt: string, calls: readonly PromptCall[]): PromptForm {
  const kind = promptKind(answers);
  const call = calls.length === 1 ? calls[0] : null;
  const offered = offeredPromptAnswers(answers, calls);
  const ordered = [...offered.filter(answer => answer !== 'deny'), ...offered.filter(answer => answer === 'deny')];
  const options = ordered.map(answer => ({ answer, label: promptAnswerLabel(harness, kind, call?.tool ?? null, answer), enter: enterAnswers(answer), refuses: refuses(answer) }));
  let mark: number | null = options.findIndex(option => option.enter);
  if (mark < 0) mark = options.findIndex(option => !option.refuses);
  if (mark < 0) mark = null;
  const elsewhere = harness === 'claude' && (kind === 'permission' || kind === 'plan')
    ? "Claude's other choices are in its terminal (Ctrl+])."
    : harness === 'codex' && kind === 'permission' ? "Codex's other choices are in its terminal (Ctrl+])." : null;
  return { kind, question: questionLine(harness, kind, prompt, call), options, mark, elsewhere };
}
