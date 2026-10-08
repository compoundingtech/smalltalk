import type { ConversationContentRef, TimelineEntry } from '@smalltalk/st3-client';
import { contentReferences } from './conversationContent.ts';

// A conversation drawn the way stui draws it (crates/stui/src/ui/adapt.rs conversation and
// from_harness, conversation.rs): the person's turns as tinted blocks, replies as markdown, tool
// calls as small boxes tinted by outcome, Small Talk in the same stream with a bar, and graph
// events and diagnostics as one dim line. Anything unexpected is shown as what it is, not dropped
// and not allowed to break the rest.

export type ToolState = 'running' | 'ok' | 'failed';
export type Body =
  | { kind: 'user'; text: string }
  | { kind: 'assistant'; text: string }
  | { kind: 'tool'; title: string; state: ToolState; output: string[] }
  /** `delivered`: the recipient's harness has it, seen in the agent's own transcript. */
  | { kind: 'mail'; from: string; to: string; subject: string; text: string; delivered?: boolean; dictated?: boolean; signed?: string; images?: MailImage[] }
  | { kind: 'event'; text: string; tone: 'quiet' | 'warning' | 'fault' };
export type ConversationEntry = { id: string; at: string; timestamp: string; body: Body; content?: ConversationContentRef[]; contentSources?: Entry[] };

// ------------------------------------------------------------ typed views (OMP parity)

/** A typed view on a block: the discriminator is `type`; parsed fields only, the native data
 * stays in the block's `payload` (contract: OMP parity, stacked on #1574). */
export type BlockView = { type: string } & Record<string, unknown>;
export type SubagentSummary = { id: string; name?: string; agent?: string; status: string; task?: string; duration_ms?: number; tokens?: number; cost_usd?: number; requests?: number; tool_count?: number; conversation?: { session_id: string } };
export type JobSummary = { id: string; name?: string; type?: string; state: string; exit_code?: number; started_at?: string; ended_at?: string; duration_ms?: number; output_bytes?: number };
/** One field of the conversation header: its value, whether the live register (`register`) or
 * the transcript window (`transcript`) holds it, and when that was true. */
export type HeaderField = { value: unknown; source: string; as_of: string };
export type ConversationHeader = Partial<Record<'model' | 'context' | 'cost' | 'todos' | 'jobs' | 'subagents' | 'ask' | 'working', HeaderField>>;

type Entry = Pick<TimelineEntry, 'id' | 'role' | 'timestamp'> & { type: string; body: unknown };
type Names = ReadonlyMap<string, string>;

function record(value: unknown): Record<string, unknown> {
  return value && typeof value === 'object' && !Array.isArray(value) ? value as Record<string, unknown> : {};
}
function str(value: unknown): string | undefined {
  return typeof value === 'string' ? value : undefined;
}

/** The typed view on the first block of `kind`, when it has one. */
function blockView(blocks: unknown, kind: string): BlockView | undefined {
  if (!Array.isArray(blocks)) return undefined;
  for (const block of blocks) {
    if (record(block).kind !== kind) continue;
    const view = record(record(block).view);
    if (typeof view.type === 'string') return view as BlockView;
  }
  return undefined;
}

/** Local `HH:MM` of an RFC 3339 timestamp, or nothing when it does not parse. */
export function clock(timestamp: string): string {
  const at = new Date(timestamp);
  if (!Number.isFinite(at.getTime())) return '';
  return `${String(at.getHours()).padStart(2, '0')}:${String(at.getMinutes()).padStart(2, '0')}`;
}

function short(id: string): string {
  return id.replace(/^mission\//, '').replace(/^agent\//, '');
}

// ------------------------------------------------------------------ text cleanup

const HIDDEN_TAGS = ['analysis', 'thinking', 'think', 'internal', 'system-reminder', 'function_calls', 'tool_result'];

function stripInternalMarkup(input: string, markup = true): string {
  if (!markup) return input;
  let inChannel = false;
  let text = input.split('\n').filter(line => {
    const trimmed = line.trim();
    if (trimmed.startsWith('<channel ') && trimmed.includes('source="plugin:st3-channel:st3"') && trimmed.endsWith('>')) { inChannel = true; return false; }
    if (inChannel && trimmed === '</channel>') { inChannel = false; return false; }
    return !(trimmed.startsWith('[st3-delivery:') && trimmed.endsWith('.md]'));
  }).join('\n');
  for (const tag of HIDDEN_TAGS) {
    let from = 0;
    for (;;) {
      const lower = text.toLowerCase(), open = `<${tag}`;
      const start = lower.indexOf(open, from);
      if (start < 0) break;
      const lineStart = text.lastIndexOf('\n', start - 1) + 1;
      const named = !/^[> /\n]/.test(lower.slice(start + open.length)) || (text.slice(lineStart,start).split('`').length - 1) % 2 === 1;
      const block = !text.slice(lineStart, start).trim();
      const openEnd = lower.indexOf('>', start), close = openEnd < 0 ? -1 : lower.indexOf(`</${tag}>`, openEnd + 1);
      if (named) from = start + open.length;
      else if (close >= 0) text = text.slice(0,start) + text.slice(close + tag.length + 3);
      else if (block) { text = text.slice(0,start); break; }
      else from = start + open.length;
    }
  }
  return text;
}

/** What a message says once the markup harnesses and st add for the model is gone. */
export type DisplayFilter = 'harness-markup' | 'context-blocks' | 'control-characters' | 'internal-blocks' | 'excerpts' | 'bookkeeping';
export const DEFAULT_FILTERS: readonly DisplayFilter[] = ['harness-markup', 'context-blocks', 'control-characters', 'internal-blocks', 'excerpts', 'bookkeeping'];
export const SHOW_EVERYTHING: readonly DisplayFilter[] = [];

// Said once at the start of a conversation st read only the newest part of.
const NATIVE_PREFIX_NOTE = "Earlier history is not shown: st reads only the newest part of this agent's transcript";

// Harness records that carry no conversation: the transcript's own titles and modes. Unknown
// records stay visible.
const BOOKKEEPING_RECORDS = [
  // Claude's own entries, system records and attachments.
  'last-prompt', 'ai-title', 'mode', 'permission-mode', 'atis-latch', 'pr-link', 'frame-link', 'bridge-session', 'queue-operation', 'cost-state',
  'stop_hook_summary', 'turn_duration', 'total_tokens_reminder', 'deferred_tools_record', 'silent_turn_reminder', 'environment', 'date', 'skill_listing', 'command_permissions', 'edited_text_file',
  // Codex's turn bookkeeping: the messages and calls they mention are entries of their own.
  'event_msg', 'token_usage_record', 'turn_context', 'world_state',
];

// A record Claude writes about its own session, in kinds that keep appearing (instructions, session
// context, prompt snapshots, deferred tools, hook summaries): an attachment, system record or entry
// no renderer knows by name. Claude's own screen shows none as conversation, so they are
// bookkeeping as a class (as in st3-conversation-ui).
const claudeContextRecord = (text: string) => /^\[unrecognized claude (attachment|system|entry) `/.test(text);

// A reasoning step whose text the model did not share is bookkeeping too.
// omp's `custom` entries that only repeat what the conversation shows: a tool call's start (its call is
// an entry of its own) and the end of the session. Other custom entries stay visible.
const OMP_BOOKKEEPING_CUSTOM = ['tool_execution_start', 'session_exit'];
const ompBookkeeping = (block: Record<string, unknown>) =>
  str(block.source_type) === 'custom' && OMP_BOOKKEEPING_CUSTOM.includes(str(record(record(block.payload).raw).customType) ?? '');

function isBookkeeping(entry: Entry, filters: readonly DisplayFilter[]): boolean {
  if (!filters.includes('bookkeeping') || entry.type !== 'content') return false;
  const body = record(entry.body);
  const shown = (Array.isArray(body.blocks) ? body.blocks : []).map(record).filter(block => !['internal', 'hidden-by-harness'].includes(str(block.visibility) ?? ''));
  if (!shown.length) return false;
  if (entry.role === 'assistant') return shown.every(block => block.kind === 'reasoning' && !(str(record(block.payload).text) ?? '').trim());
  if (entry.role === 'system') return shown.every(block => block.kind === 'unknown' && (BOOKKEEPING_RECORDS.includes(str(block.source_type) ?? '') || ompBookkeeping(block) || claudeContextRecord(str(body.text) ?? '')));
  return false;
}

export function cleanMessageText(raw: string, filters: readonly DisplayFilter[] = DEFAULT_FILTERS): string {
  if (!filters.length) return raw;
  const markup = filters.includes('harness-markup');
  const normalized = raw.replace(/\r\n/g, '\n');
  let output = '', plain = '', code = false;
  for (const line of normalized.split(/(?<=\n)/)) {
    if (line.trimStart().startsWith('```')) {
      if (!code) { output += stripInternalMarkup(plain, markup); plain = ''; }
      output += line;
      code = !code;
    } else if (code) output += line;
    else plain += line;
  }
  output += stripInternalMarkup(plain, markup);
  // eslint-disable-next-line no-control-regex
  const safe = (filters.includes('control-characters') ? output.replace(/[\u0000-\u0008\u000b-\u001f\u007f]/g, '') : output).trim();
  const text = (safe.startsWith('[PING] ?') ? safe.slice('[PING] ?'.length) : safe).trim();
  const reference = text.lastIndexOf(' [id:message/');
  return reference >= 0 && text.endsWith(']') ? text.slice(0, reference).trimEnd() : text;
}

const CONTEXT_BLOCKS = ['system-reminder', 'local-command-caveat', 'environment_context', 'permissions', 'collaboration_mode', 'multi_agent_mode', 'apps_instructions', 'plugins_instructions', 'skills_instructions', 'user_instructions', 'developer_instructions', 'command-message', 'command-args'];

function filterContextBlocks(text: {value:string}): void {
  const inCode = (at:number) => (text.value.slice(text.value.lastIndexOf('\n',at-1)+1,at).split('`').length-1)%2 === 1;
  for (const tag of CONTEXT_BLOCKS) {
    const open = `<${tag}`, close = `</${tag}>`; let from = 0;
    for (;;) {
      const start = text.value.indexOf(open,from);
      if (start < 0) break;
      if (inCode(start) || !/^[> /\n]/.test(text.value.slice(start+open.length))) { from = start+open.length; continue; }
      const headEnd = text.value.indexOf('>',start);
      if (headEnd < 0) { text.value = text.value.slice(0,start); break; }
      let end = text.value.indexOf(close,headEnd+1);
      while (end >= 0 && inCode(end)) end = text.value.indexOf(close,end+close.length);
      text.value = text.value.slice(0,start) + (end < 0 ? '' : text.value.slice(end+close.length)); from = start;
    }
  }
}

/** Take every `<tag …>…</tag>` block out of `text`; a block that never closes runs to the end. */
function takeBlocks(text: { value: string }, tag: string): string[] {
  const found: string[] = [];
  const open = `<${tag}`, close = `</${tag}>`;
  let from = 0;
  for (;;) {
    const start = text.value.indexOf(open, from);
    if (start < 0) break;
    const after = text.value.slice(start + open.length);
    if (!/^[> /\n]/.test(after)) { from = start + open.length; continue; }
    const headEnd = after.indexOf('>');
    if (headEnd < 0) { text.value = text.value.slice(0, start); break; }
    const bodyStart = start + open.length + headEnd + 1;
    const end = text.value.indexOf(close, bodyStart);
    found.push(end < 0 ? text.value.slice(bodyStart) : text.value.slice(bodyStart, end));
    text.value = text.value.slice(0, start) + (end < 0 ? '' : text.value.slice(end + close.length));
    from = start;
  }
  return found;
}
/** An attribute of a `<tag …>` head, unescaped. */
function attribute(head: string, name: string): string | undefined {
  const value = new RegExp(` ${name}="([^"]*)"`).exec(head)?.[1];
  return value === undefined ? undefined : unescapeXml(value);
}
/** Undo the XML escaping st applies to a delivery's attributes and body. */
function unescapeXml(text: string): string {
  return text.replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&quot;/g, '"').replace(/&apos;/g, "'").replace(/&amp;/g, '&');
}
function field(block: string, tag: string): string | undefined {
  return takeBlocks({ value: block }, tag)[0]?.trim();
}
function shorten(text: string, max: number): string {
  const line = (text.split('\n')[0] ?? '').trim();
  return [...line].length <= max ? line : `${[...line].slice(0, max).join('')}…`;
}

/** The graph message a `[PING from st3] message/ID from …` line announces. */
function pingId(text: string): string | undefined {
  return /^\s*\[PING from st3\] (message\/\S+) from /m.exec(text)?.[1];
}

/**
 * A user or system entry from a harness transcript, turned into what a person should see.
 * A delivery of mail the stream already shows (`shown`) is not announced again: its id goes in
 * `delivered`, and the mail itself says it arrived.
 */
/**
 * How a message's signature reads beside its sender: the device that signed it and whether that
 * checks ("✓ example phone (secure enclave)"). A message with no signature is usually just old, so it
 * says nothing. The same words as stui's `signature_mark`.
 */
export function signatureMark(provenance: unknown): string | undefined {
  if (!provenance || typeof provenance !== 'object') return undefined;
  const p = provenance as { verdict?: string; reason?: string; signer?: string; device?: string };
  const who = p.device ?? p.signer ?? '';
  switch (p.verdict) {
    case 'verified': return `✓ ${who}`.trimEnd();
    case 'held': return `⚠ ${p.reason ? `signature held: ${p.reason}` : 'signature held'}`;
    case 'invalid': return `✕ ${p.reason ? `signature invalid: ${p.reason}` : 'signature invalid'}`;
    default: return undefined;
  }
}

/** The lines st adds beside a delivery for the agent (st-drivers `ding`). */
const ST_DELIVERY_NOTES = [
  "The person reads replies in st, not in the agent's session.",
  '(dictated by voice; it may contain transcription mistakes)',
];

export function fromHarness(isUser: boolean, raw: string, shown: ReadonlySet<string> = new Set(), delivered: Set<string> = new Set(), filters: readonly DisplayFilter[] = DEFAULT_FILTERS): Body[] {
  // A harness that ran out of context continues from a summary written as the person's turn: the
  // agent's own notes, kilobytes long. One folded line that opens like a tool call, as stui shows it.
  if (isUser && raw.includes('This session is being continued from a previous conversation')) {
    const output = raw.replace(/\r\n/g, '\n').split('\n').filter(line => !/^\s*<[A-Za-z0-9_-]+\/>\s*$/.test(line));
    return [{ kind: 'tool', title: 'context summary · the conversation was compacted', state: 'ok', output }];
  }
  const text = { value: raw.replace(/\r\n/g, '\n') };
  const bodies: Body[] = [];
  for (const block of takeBlocks(text, 'task-notification')) {
    bodies.push({ kind: 'event', tone: 'quiet', text: `background task ${field(block, 'status') ?? 'update'}: ${shorten(field(block, 'summary') ?? '', 90)}` });
  }
  // st's own envelope, as Codex and the pi family receive it: the mail, or a delivery of mail the
  // stream already shows (Nathan, 2026-10-03: raw XML in a Codex seat's conversation).
  const enveloped = new Set<string>();
  const envelopeHeads = [...text.value.matchAll(/<smalltalk-message\b[^>]*>/g)].map(match => match[0]);
  takeBlocks(text, 'smalltalk-message').forEach((block, index) => {
    const head = envelopeHeads[index] ?? '';
    const graph = attribute(head, 'graph');
    if (graph) enveloped.add(graph);
    if (graph && shown.has(graph)) { delivered.add(graph); return; }
    bodies.push({ kind: 'mail', from: attribute(head, 'from') ?? 'someone', to: attribute(head, 'to') ?? '', subject: attribute(head, 'subject') ?? '', text: cleanMessageText(unescapeXml(block), filters).trim() });
  });
  const channelHeads = [...text.value.matchAll(/<channel\b[^>]*>/g)].map(match => match[0]);
  const senders = channelHeads.map(head => attribute(head, 'from') ?? 'someone');
  takeBlocks(text, 'channel').forEach((block, index) => {
    // The delivered message: its PING line, the message the channel names, or the st envelope
    // it carries. One the stream shows is marked delivered, never announced again.
    const envelope = /<smalltalk-message\b[^>]*>/.exec(block)?.[0];
    const id = pingId(block) ?? attribute(channelHeads[index] ?? '', 'messageId') ?? (envelope ? attribute(envelope, 'graph') : undefined);
    if (id && shown.has(id)) { delivered.add(id); return; }
    // Its envelope was read above: that is the mail; the rest is the delivery's own notes.
    if (id && enveloped.has(id)) return;
    // Mail the stream does not show reads as that mail, never as the delivery's other lines
    // (Nathan, 2026-10-03: "delivered to the agent: The person reads replies in st…").
    if (envelope) {
      const inner = takeBlocks({ value: block }, 'smalltalk-message')[0] ?? '';
      bodies.push({ kind: 'mail', from: attribute(envelope, 'from') ?? senders[index] ?? 'someone', to: attribute(envelope, 'to') ?? '', subject: attribute(envelope, 'subject') ?? '', text: cleanMessageText(unescapeXml(inner), filters).trim() });
      return;
    }
    const subject = block.split('\n').map(line => line.trim()).find(line => line.startsWith('Subject:'))?.slice('Subject:'.length).trim() ?? shorten(cleanMessageText(block, filters), 70);
    bodies.push({ kind: 'event', tone: 'quiet', text: `delivered to the agent: ${shorten(subject, 80)} · from ${senders[index] ?? 'someone'}` });
  });
  // `[PING from st3] message/ID from SENDER: TITLE` announces mail on its own line: mail the
  // stream shows is marked delivered; otherwise it is one quiet line, as in stui.
  const pings: Body[] = [];
  text.value = text.value.split('\n').filter(line => {
    const id = pingId(line);
    if (id && shown.has(id)) { delivered.add(id); return false; }
    const ping = /^\s*\[PING from st3\] \S+ from (\S+): (.*)$/.exec(line);
    if (!ping) return true;
    pings.push({ kind: 'event', tone: 'quiet', text: `delivered to the agent: ${shorten(ping[2], 80)} · from ${short(ping[1])}` });
    return false;
  }).join('\n');
  bodies.push(...pings);
  for (const command of takeBlocks(text, 'command-name')) {
    bodies.push({ kind: 'user', text: `${command.trim()} ${field(raw, 'command-args') ?? ''}`.trim() });
  }
  for (const [tag, state] of [['local-command-stdout', 'ok'], ['local-command-stderr', 'failed']] as const) {
    for (const output of takeBlocks(text, tag)) bodies.push({ kind: 'tool', title: 'command output', state, output: output.trim().split('\n') });
  }
  for (const _ of takeBlocks(text, 'turn_aborted')) bodies.push({ kind: 'event', tone: 'quiet', text: 'the turn was interrupted' });
  for (const reply of takeBlocks(text, 'send_user_message_question_reply')) {
    let answers: string[] = [];
    try {
      const parsed: unknown = JSON.parse(reply.trim());
      if (Array.isArray(parsed)) answers = parsed.map(item => str(record(item).answer)).filter((answer): answer is string => !!answer);
    } catch { /* not JSON: nothing to show */ }
    if (answers.length) bodies.push({ kind: 'user', text: answers.join('\n') });
  }
  if (raw.includes("<command-name")) takeBlocks(text, "command-args"); // displayed with the parsed command
  // st's own notes beside a delivery tell the agent something; the person never typed them.
  text.value = text.value.split('\n').filter(line => !ST_DELIVERY_NOTES.includes(line.trim())).join('\n');
  if (filters.includes('context-blocks')) filterContextBlocks(text);
  const rest = cleanMessageText(text.value, filters);
  if (rest) {
    if (isUser) bodies.unshift({ kind: 'user', text: rest });
    else bodies.push({kind:'event',tone:'quiet',text:rest});
  }
  return bodies;
}

// ------------------------------------------------------------------ tools

const TITLE_KEYS = ['command', 'cmd', 'file_path', 'path', 'pattern', 'url', 'query', 'description'];
export function toolTitle(name: string, args: unknown): string {
  let value = args;
  if (typeof value === 'string') { try { value = JSON.parse(value); } catch { /* keep the string */ } }
  const picked = TITLE_KEYS.map(key => str(record(value)[key])).find(Boolean);
  if (!picked) return name || 'tool';
  const first = picked.split('\n')[0];
  return ['Bash', 'bash', 'shell', 'exec_command'].includes(name) ? `$ ${first}` : `${name} ${first}`;
}

export function toolOutput(content: unknown, full = false): string[] {
  let text: string;
  if (typeof content === 'string') text = content;
  else if (Array.isArray(content)) text = content.map(item => str(record(item).text) ?? str(item)).filter((part): part is string => part !== undefined).join('\n');
  else if (content == null) text = '';
  else text = str(record(content).text) ?? JSON.stringify(content);
  // As st3-conversation-ui: no blank lines around the output.
  const lines = text ? text.split('\n') : [];
  while (lines.length && !lines[0].trim()) lines.shift();
  while (lines.length && !lines[lines.length - 1].trim()) lines.pop();
  return full ? lines : lines.slice(0, 400);
}

function contentText(body: unknown): string {
  if (typeof body === 'string') return body;
  return str(record(body).text) ?? '';
}

// ------------------------------------------------------------ typed rows (OMP parity)
// The rows a typed view becomes, mirroring st3-conversation-ui exactly: same titles, same
// lines, so the phone and stui read one conversation the same way (fixtures/clients/transcripts).

/** The title of a tool call its typed view names: `$ command`, `edit path`, `write path · N bytes`,
 * `read path:range`, `engine pattern query path`, `todo op`, `ask`, `task · N agents`,
 * `hub op name target`, `eval language · title`, or the generic tool's name. */
function viewTitle(view: BlockView, tool: string, args: unknown): string {
  const tasks = Array.isArray(view.tasks) ? view.tasks.length : undefined;
  const bytes = typeof view.bytes === 'number' ? view.bytes : undefined;
  const title: string | undefined = (() => {
    switch (view.type) {
      case 'bash': return str(view.command) !== undefined ? `$ ${str(view.command)}` : undefined;
      case 'edit': return str(view.path) !== undefined ? `edit ${str(view.path)}` : undefined;
      case 'write': return `write ${str(view.path) ?? ''}${typeof view.line_count === 'number' ? ` · ${view.line_count} lines` : ''}${bytes !== undefined ? ` · ${bytes} bytes` : ''}`.trim();
      case 'read': return `read ${str(view.path) ?? ''}${str(view.range) ? `:${str(view.range)}` : ''}`.trim();
      case 'search': return [str(view.engine), str(view.pattern) ?? str(view.query), str(view.path)].filter(Boolean).join(' ');
      case 'todo': return `todo ${str(view.op) ?? ''}`.trim();
      case 'ask': return 'ask';
      case 'task': return `task · ${tasks ?? 0} agents`;
      case 'hub': return `hub ${[str(view.op), str(view.name), str(view.target)].filter(Boolean).join(' ')}`.trim();
      case 'eval': return `eval ${str(view.language) ?? ''}${str(view.title) ? ` · ${str(view.title)}` : ''}`.trim();
      case 'generic': return str(view.name);
      default: return undefined;
    }
  })();
  return title || toolTitle(tool, args);
}

function viewCallLines(view: BlockView): string[] {
  const lines: string[] = [];
  const fields = view.type === 'bash' ? ['cwd', 'timeout_s']
    : view.type === 'search' ? ['case', 'hidden', 'gitignore', 'limit', 'skip']
    : view.type === 'eval' ? ['language', 'timeout_s', 'reset'] : [];
  for (const field of fields) {
    const value = view[field];
    if (typeof value === 'string' || typeof value === 'number' || typeof value === 'boolean') {
      lines.push(`${field === 'timeout_s' ? 'timeout' : field}: ${value}${field === 'timeout_s' ? 's' : ''}`);
    }
  }
  if (view.type === 'write') lines.push(...(str(view.content)?.split('\n') ?? []));
  if (view.type === 'eval') lines.push(...(str(view.code)?.split('\n') ?? []));
  if (view.type === 'hub') lines.push(...(str(view.message)?.split('\n') ?? []));
  if (view.type === 'task') {
    if (str(view.context)) lines.push('parent context / contract:', ...str(view.context)!.split('\n'));
    for (const task of Array.isArray(view.tasks) ? view.tasks : []) {
      const value = record(task);
      lines.push([str(value.name), str(value.agent)].filter(Boolean).join(' · '), ...(str(value.task)?.split('\n') ?? []));
    }
  }
  if (view.type === 'ask') {
    for (const question of Array.isArray(view.questions) ? view.questions : []) {
      const value = record(question);
      lines.push(str(value.question) ?? '');
      for (const [index, option] of (Array.isArray(value.options) ? value.options : []).entries()) {
        const alternative = record(option);
        lines.push(`  ${index + 1}. ${str(alternative.label) ?? ''}`);
        if (str(alternative.description)) lines.push(...str(alternative.description)!.split('\n').map(line => `     ${line}`));
      }
    }
  }
  return lines;
}

function markSelected(lines: readonly string[], view: BlockView | undefined): string[] {
  if (view?.type !== 'ask') return [...lines];
  const selected = new Set((Array.isArray(view.answers) ? view.answers : []).flatMap(answer => {
    const values = record(answer).selected;
    return Array.isArray(values) ? values.filter((value): value is string => typeof value === 'string') : [];
  }));
  return lines.map(line => {
    const label = /^\s+\d+\. (.*)$/.exec(line)?.[1];
    return label !== undefined && selected.has(label) ? `${line} [selected]` : line;
  });
}

/** One background job as a person reads it: `name · state · exit N · Nms`. */
function jobLine(job: unknown): string {
  const value = record(job);
  const exit = typeof value.exit_code === 'number' ? `exit ${value.exit_code}` : undefined;
  const took = typeof value.duration_ms === 'number' ? `${value.duration_ms}ms` : undefined;
  return [str(value.name) ?? str(value.id), str(value.state), exit, took].filter(Boolean).join(' · ');
}

const todoMark = (status: string | undefined) => status === 'completed' ? '[x]' : status === 'in_progress' ? '[~]' : status === 'pending' ? '[ ]' : status === 'blocked' ? '[!]' : status === 'abandoned' ? '[/]' : '[-]';

/** A todo list as lines: each phase's name, then its items as `[x] content`. */
function todoLines(view: BlockView): string[] {
  const lines: string[] = [];
  for (const phase of Array.isArray(view.phases) ? view.phases : []) {
    const named = str(record(phase).name);
    if (named) lines.push(named);
    const items = record(phase).items;
    for (const item of Array.isArray(items) ? items : []) lines.push(`${todoMark(str(record(item).status))} ${str(record(item).content) ?? ''}`);
  }
  return lines;
}

/** Answers to an ask as lines: each question, then what was selected, typed, or noted. */
function askLines(view: BlockView): string[] {
  const lines: string[] = [];
  for (const answer of Array.isArray(view.answers) ? view.answers : []) {
    const value = record(answer);
    if (str(value.question)) lines.push(str(value.question)!);
    const selected = Array.isArray(value.selected) ? value.selected.filter((part): part is string => typeof part === 'string') : [];
    if (selected.length) lines.push(`  selected: ${selected.join(', ')}`);
    if (str(value.custom)) lines.push(`  custom: ${str(value.custom)}`);
    if (str(value.note)) lines.push(`  note: ${str(value.note)}`);
  }
  return lines;
}

/** What a bash outcome says on one line: `exit 0 · 12ms`, `timed out after 30s`. */
function bashLine(view: BlockView): string | undefined {
  const parts: string[] = [];
  if (typeof view.exit_code === 'number') parts.push(`exit ${view.exit_code}`);
  if (typeof view.wall_ms === 'number') parts.push(`${view.wall_ms}ms`);
  if (view.timed_out === true) parts.push(typeof view.timeout_s === 'number' ? `timed out after ${view.timeout_s}s` : 'timed out');
  return parts.length ? parts.join(' · ') : undefined;
}

/** The lines a tool's typed output view shows, replacing the raw result text; `undefined` keeps
 * the raw text (views that add nothing, or a type this app does not know). */
function viewOutput(view: BlockView, content: unknown, full = false): string[] | undefined {
  switch (view.type) {
    case 'bash': {
      const line = bashLine(view);
      return line ? [...toolOutput(content, full), line] : toolOutput(content, full);
    }
    case 'search': {
      const summary = [
        typeof view.match_count === 'number' ? `${view.match_count} matches` : undefined,
        typeof view.file_count === 'number' ? `${view.file_count} files` : undefined,
      ].filter(Boolean).join(' / ');
      return [
        ...(summary ? [summary] : []),
        ...(view.truncated === true ? ['warning: search results truncated'] : []),
        ...(typeof view.file_limit_reached === 'number' ? [`warning: file limit reached (${view.file_limit_reached})`] : []),
        ...(typeof view.per_file_limit_reached === 'number' ? [`warning: per-file limit reached (${view.per_file_limit_reached})`] : []),
        ...(str(view.warning)?.split('\n') ?? []),
        ...toolOutput(content, full),
      ];
    }
    case 'edit': return str(view.diff) !== undefined ? str(view.diff)!.split('\n') : toolOutput(content, full);
    case 'todo': return Array.isArray(view.phases) ? todoLines(view) : undefined;
    case 'ask': return Array.isArray(view.answers) ? askLines(view) : undefined;
    case 'task': return [];
    case 'hub': return Array.isArray(view.jobs) ? view.jobs.map(jobLine) : undefined;
    default: return undefined;
  }
}

/** A subagent result as its own card: `agent · status`, what it used, and the conversation
 * it opens (`open session/…`). The task assignment belongs only to the invocation card. */
function subagentCard(agent: unknown, state: ToolState): { id: string; title: string; output: string[] } {
  const value = record(agent);
  const lines: string[] = [];
  if (typeof value.duration_ms === 'number') lines.push(`duration ${value.duration_ms}ms`);
  if (typeof value.tokens === 'number') lines.push(`tokens ${value.tokens}`);
  if (typeof value.cost_usd === 'number') lines.push(`cost $${value.cost_usd.toFixed(2)}`);
  const session = str(record(value.conversation).session_id);
  if (session) lines.push(`open ${session}`);
  return { id: str(value.id) ?? '', title: [str(value.name) ?? str(value.agent) ?? str(value.id), str(value.status), ...(str(value.name) && str(value.agent) ? [str(value.agent)] : [])].filter(Boolean).join(' · '), output: lines };
}

/** The child conversation a subagent card opens, when it has one. */
export function subagentSession(output: readonly string[]): string | undefined {
  for (const line of output) {
    const session = /^open (\S+)$/.exec(line.trim())?.[1];
    if (session) return session;
  }
  return undefined;
}

/** The one line a status block's view becomes; `undefined` when the view says nothing this app
 * knows, `null` when it must not be drawn at all (`tool_start` belongs to its call). */
function statusLine(view: BlockView): string | null | undefined {
  switch (view.type) {
    case 'tool_start': return null;
    case 'compaction': {
      const before = typeof view.tokens_before === 'number' ? view.tokens_before : undefined;
      const after = typeof view.tokens_after === 'number' ? view.tokens_after : undefined;
      return ['compaction', str(view.method), before !== undefined && after !== undefined ? `${before} → ${after} tokens` : undefined].filter(Boolean).join(' · ');
    }
    case 'model_change': return ['model', str(view.model)].filter(Boolean).join(' · ');
    case 'thinking_level': return ['thinking', str(view.level)].filter(Boolean).join(' · ');
    case 'reset_boundary': return 'session reset';
    case 'credential_pin': return ['credential pin', str(view.provider)].filter(Boolean).join(' · ');
    case 'title': return ['title', str(view.title)].filter(Boolean).join(' · ');
    case 'session_exit': return ['session exit', str(view.kind), str(view.reason)].filter(Boolean).join(' · ');
    case 'skill': return ['skill', str(view.name), str(view.path)].filter(Boolean).join(' · ');
    default: return undefined;
  }
}

/** What the extension blocks (#1574 kinds `irc`, `job`, `status`) draw, role aside: a recognized
 * view replaces the entry's text fallback entirely. `skip` says draw nothing (`tool_start`). */
function extensionBodies(blocks: unknown): Body[] | 'skip' | undefined {
  if (!Array.isArray(blocks)) return undefined;
  const bodies: Body[] = [];
  let skip = false;
  for (const block of blocks) {
    const value = record(block);
    const view = record(value.view);
    if (typeof view.type !== 'string') continue;
    if (value.kind === 'irc' && view.type === 'irc') {
      // An incoming IRC message is mail like a delivery's: sent (✓), not a graph receipt (✓✓).
      bodies.push({ kind: 'mail', from: str(view.from) ?? 'someone', to: '', subject: '', text: str(view.message) ?? '', delivered: false });
    } else if (value.kind === 'job' && view.type === 'job' && Array.isArray(view.jobs)) {
      for (const job of view.jobs) bodies.push({ kind: 'event', tone: 'quiet', text: jobLine(job) });
    } else if (value.kind === 'error' && view.type === 'assistant_error') {
      if (view.presentation !== 'none') bodies.push({
        kind: 'tool', title: `assistant error · ${str(view.status) ?? 'failed'} · ${str(view.label) ?? ''}`,
        state: view.is_error === true ? 'failed' : 'ok',
        output: [...(str(view.message)?.split('\n') ?? []), ...(str(record(view.retry).note)?.split('\n') ?? [])],
      });
      else skip = true;
    } else if (value.kind === 'status' && view.type === 'compaction') {
      bodies.push({ kind: 'tool', title: statusLine(view as BlockView) ?? 'compaction', state: 'ok', output: str(view.summary)?.split('\n') ?? [] });
    } else if (value.kind === 'status') {
      const line = statusLine(view as BlockView);
      if (line === null) skip = true;
      else if (line !== undefined && line !== '') bodies.push({ kind: 'event', tone: 'quiet', text: line });
    }
  }
  return bodies.length ? bodies : skip ? 'skip' : undefined;
}

/** How long ago `as_of` was, as a person reads it: `0s`, `5m`, `3h`, `2d`. */
function headerAge(asOf: string, now: string): string | undefined {
  const at = Date.parse(asOf), current = Date.parse(now);
  if (!Number.isFinite(at) || !Number.isFinite(current)) return undefined;
  const seconds = Math.max(0, Math.floor((current - at) / 1000));
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor((seconds + 30) / 60);
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.floor((seconds + 1800) / 3600);
  return hours < 24 ? `${hours}h` : `${Math.floor((seconds + 43200) / 86400)}d`;
}

/** Every present field in fixed order, with shared provenance once and minority sources
 * marked individually. The shared age is the oldest field from that source, as in stui. */
export function headerLine(header: ConversationHeader | undefined, now: string): string | null {
  if (!header) return null;
  const fields: HeaderField[] = [];
  const marker = (field: HeaderField): string => {
    const age = headerAge(field.as_of, now);
    return `${field.source}${age ? ` · ${age} ago` : ''}`;
  };
  const marked = (label: string, field: HeaderField): string | undefined => {
    if (field === undefined) return undefined;
    fields.push(field);
    return label.trim();
  };
  const parts: string[] = [];
  const model = str(header.model?.value) ? marked(`model ${str(header.model?.value)}`, header.model!) : undefined;
  if (model) parts.push(model);
  const tokens = record(header.context?.value).tokens;
  const context = typeof tokens === 'number' ? marked(`context ${tokens} tokens`, header.context!) : undefined;
  if (context) parts.push(context);
  const usd = record(header.cost?.value).usd;
  const cost = typeof usd === 'number' ? marked(`cost $${usd.toFixed(2)}`, header.cost!) : undefined;
  if (cost) parts.push(cost);
  const items = Array.isArray(header.todos?.value) ? header.todos!.value.flatMap(phase => Array.isArray(record(phase).items) ? record(phase).items : []) : [];
  if (items.length || header.todos !== undefined) {
    const done = items.filter(item => record(item).status === 'completed').length;
    const todo = marked(`todo ${done}/${items.length}`, header.todos!);
    if (todo) parts.push(todo);
  }
  const jobs = Array.isArray(header.jobs?.value) ? marked(`jobs ${header.jobs!.value.length}`, header.jobs!) : undefined;
  if (jobs) parts.push(jobs);
  const subagents = Array.isArray(header.subagents?.value) ? marked(`agents ${header.subagents!.value.length}`, header.subagents!) : undefined;
  if (subagents) parts.push(subagents);
  const pending = record(header.ask?.value);
  const question = Array.isArray(pending.questions) ? str(pending.questions.map(record).find(question => str(question.question) !== undefined)?.question) : undefined;
  const ask = question !== undefined ? marked(`ask ${question}`, header.ask!) : undefined;
  if (ask) parts.push(ask);
  const working = typeof header.working?.value === 'boolean' ? marked(header.working.value ? 'working' : 'idle', header.working!) : undefined;
  if (working) parts.push(working);
  if (!parts.length) return null;
  let common = '', commonCount = 0;
  for (const field of fields) {
    const count = fields.filter(other => other.source === field.source).length;
    if (field.source && count > commonCount) {
      common = field.source;
      commonCount = count;
    }
  }
  const timestamp = (field: HeaderField): number => {
    const at = Date.parse(field.as_of);
    return Number.isFinite(at) ? at : -Infinity;
  };
  const shared = fields.filter(field => field.source === common);
  const oldest = shared.reduce<HeaderField | undefined>((old, field) => old === undefined || timestamp(field) < timestamp(old) ? field : old, undefined);
  const rendered = parts.map((part, index) => fields[index].source && fields[index].source !== common ? `${part} [${marker(fields[index])}]` : part);
  if (oldest && common) rendered.push(marker(oldest));
  return rendered.join(' · ');
}

// ------------------------------------------------------------------ entries

/** Which timeline entries a conversation keeps: everything but heartbeats and usage. */
export function isShown(entry: { type: string }): boolean {
  return entry.type !== 'status' && entry.type !== 'usage';
}

function diagnosticTone(body: Record<string, unknown>): 'quiet' | 'warning' | 'fault' {
  const details = record(body.details);
  const code = str(body.code) ?? '';
  // A recovery is good news, and a warning is not a failure; only an error reads as one.
  if (code.endsWith('-recovered') || str(details.status) === 'recovered') return 'quiet';
  if (details.severity === 'warning' || body.retryable === true) return 'warning';
  return 'fault';
}

/**
 * Why a conversation cannot be shown whole: st could not read the harness's transcript, and sent
 * only the Small Talk around it. Half a conversation reads as the agent ignoring the person, so
 * neither half shows; this says why, with the transcript's path so it can be reported (Nathan,
 * 2026-10-01). As stui's `unreadable_transcript`.
 */
export function unreadableTranscript(timeline: Entry[]): string | null {
  for (const entry of [...timeline].reverse()) {
    if (entry.type !== 'error') continue;
    const body = record(entry.body);
    if (str(body.code) !== 'transcript-not-bound') continue;
    // A seat that has said nothing since it started has no transcript yet; that is not a failure.
    if (record(body.details).not_yet === true) return null;
    const reason = (str(body.message) ?? '').replace(/^transcript not bound: /, '');
    const path = str(record(body.details).transcript);
    return path ? `This conversation could not be loaded: ${reason} (transcript ${path})` : `This conversation could not be loaded: ${reason}`;
  }
  return null;
}

/**
 * One conversation as st joined it: the harness's turns and the agent's Small Talk, in time
 * order. `names` maps graph ids to what a person calls them; the viewer is `you`.
 */
/** An image a message carries; its bytes are read from st by `sha256`, naming `message`. */
export type MailImage = { sha256: string; message: string; mediaType: string; name?: string; size: number };

function mailImages(message: Record<string, unknown>): MailImage[] {
  const id = str(message.message_id) ?? '';
  const list = Array.isArray(message.attachments) ? message.attachments : [];
  return list.flatMap((item): MailImage[] => {
    const attachment = item as Record<string, unknown>;
    const sha256 = str(attachment.sha256), mediaType = str(attachment.media_type);
    if (!sha256 || !mediaType?.startsWith('image/')) return [];
    const name = str(attachment.name);
    return [{ sha256, message: id, mediaType, ...(name ? { name } : {}), size: typeof attachment.size === 'number' ? attachment.size : 0 }];
  });
}

export function conversationEntries(timeline: Entry[], names: Names, filters: readonly DisplayFilter[] = DEFAULT_FILTERS, fullOutput = false): ConversationEntry[] {
  if (!filters.length) return timeline.map(entry => ({id: entry.id, at: clock(entry.timestamp), timestamp: entry.timestamp, body: {kind: 'user', text: JSON.stringify(entry, null, 2)}}));
  // Provenance alone does not hide visible prose; only internal content has no row.
  if (filters.includes('internal-blocks')) timeline = timeline.filter(entry => {
    const blocks = record(entry.body).blocks;
    return !Array.isArray(blocks)
      || !blocks.some(block => record(block).kind !== 'source_record')
      || !blocks.every(block => record(block).kind === 'source_record' || ['internal', 'hidden-by-harness'].includes(str(record(block).visibility) ?? ''));
  });
  timeline = timeline.map(entry => {
    if (entry.body === null || typeof entry.body !== 'object' || Array.isArray(entry.body)) return entry;
    const body = {...record(entry.body)};
    if (filters.includes('internal-blocks') && Array.isArray(body.blocks)) body.blocks = body.blocks.filter(block => !['internal', 'hidden-by-harness'].includes(str(record(block).visibility) ?? ''));
    // #1574 exposes native data as blocks (reasoning text, raw JSON, tool output); where it does,
    // the text is that data and the markup cleanup must not eat it. stui cleans the same nothing.
    const native = Array.isArray(body.blocks) && body.blocks.some(block => record(block).kind !== 'text');
    if (typeof body.text === 'string') {
      const text = {value: body.text};
      if (filters.includes('bookkeeping') && entry.role === 'assistant' && body.text.startsWith('[reasoning]')) text.value = `Thinking · ${body.text.slice('[reasoning]'.length).trim()}`;
      if (!native && filters.includes('context-blocks') && !['user','system'].includes(entry.role)) filterContextBlocks(text);
      if (!native && !['user','system'].includes(entry.role)) text.value = cleanMessageText(text.value, filters);
      if (filters.includes('excerpts') && body.text.startsWith('[unrecognized ') && [...text.value].length > 512) text.value = [...text.value].slice(0,512).join('') + '…';
      body.text = text.value;
    }
    if (entry.type === 'error' && Array.isArray(body.blocks)) {
      const raw = body.blocks.filter(block => record(block).kind === 'raw_text').map(block => str(record(record(block).payload).text) ?? '').join('\n');
      if (raw) body.message = `${str(body.message) ?? ''}\n${cleanMessageText(raw, filters)}`;
    }
    return {...entry, body} as Entry;
  });
  const name = (id: string): string => names.get(id) ?? (id === 'daemon/runtime' ? 'st' : id.startsWith('person/') ? id.slice('person/'.length) : short(id));
  const stamped: ConversationEntry[] = [];
  const tools = new Map<string, number>();
  const callSources = new Map<string, Entry>();
  const shown = new Set(timeline.filter(entry => entry.type === 'message').map(entry => str(record(entry.body).message_id) ?? '').filter(id => id.startsWith('message/')));
  const delivered = new Set<string>();
  let mail: Record<string, unknown> | undefined;
  const push = (entry: Entry, id: string, body: Body) => {
    const content = contentReferences(entry.body);
    stamped.push({ id, at: clock(entry.timestamp), timestamp: entry.timestamp, body, ...(content.length ? { content, contentSources: [entry] } : {}) });
  };
  // Entries arrive in st's order (applyConversation keeps them by time, then sequence).
  for (const entry of timeline) {
    if (!mail && isBookkeeping(entry, filters)) continue;
    const body = record(entry.body);
    if (entry.type === 'message') {
      if (Array.isArray(body.blocks)) for (const block of body.blocks.filter(block => record(block).kind === 'raw_text')) push(entry, str(record(block).id) ?? entry.id, {kind:'event',tone:'quiet',text:`[unreadable message]\n${cleanMessageText(str(record(record(block).payload).text) ?? '', filters)}`});
      // A Small Talk message is two entries: who wrote to whom, then what they wrote. Only a
      // graph message, `message/…`, is Small Talk; a transcript heads its own turns this way too.
      mail = (str(body.message_id) ?? '').startsWith('message/') ? body : undefined;
      continue;
    }
    if (mail && entry.type === 'content') {
      const message = mail;
      mail = undefined;
      const text = cleanMessageText(contentText(entry.body), filters);
      const from = str(message.from) ?? '';
      if (from === 'daemon/runtime') {
        // Step-ready pings are graph events, not conversation.
        push(entry, str(message.message_id)!, { kind: 'event', tone: 'quiet', text: str(message.title) ?? text.split('\n')[0] ?? '' });
      } else {
        const to = str(message.to);
        const images = mailImages(message);
        push(entry, str(message.message_id)!, {
          kind: 'mail',
          from: !from && !to ? 'Small Talk' : name(from),
          to: !from && !to ? '' : name(to ?? ''),
          subject: str(message.title) ?? '',
          // A message may be only its images.
          text: text || (images.length ? '' : '(notification)'),
          // Spoken, then transcribed: marked so a reader allows for transcription mistakes.
          ...(Array.isArray(message.tags) && message.tags.includes('dictated') ? { dictated: true } : {}),
          ...(signatureMark(message.provenance) ? { signed: signatureMark(message.provenance)! } : {}),
          ...(images.length ? { images } : {}),
        });
      }
      const content = contentReferences(message);
      const last = stamped[stamped.length - 1];
      if (last && content.length) last.content = [...(last.content ?? []), ...content];
      continue;
    }
    mail = undefined;
    switch (entry.type) {
      case 'content': {
        const raw = contentText(entry.body) || (contentReferences(entry.body).length ? '[content available on demand]' : '');
        const nativeBlocks = Array.isArray(body.blocks) && body.blocks.some(block => record(block).kind !== 'text');
        // Typed extension blocks (irc, job, status) replace the entry's text fallback, role
        // aside; `tool_start` belongs to its call and is never drawn alone.
        const extension = extensionBodies(body.blocks);
        if (extension === 'skip') break;
        if (extension) {
          extension.forEach((part, index) => push(entry, extension.length === 1 ? entry.id : `${entry.id}#${index}`, part));
          break;
        }
        if ((entry.role === 'user' || entry.role === 'system') && (nativeBlocks || raw.startsWith('[unrecognized '))) {
          // A native entry's text is data #1574 exposed; it is shown as it arrived.
          const shown = nativeBlocks ? raw : cleanMessageText(raw, filters);
          push(entry, entry.id, entry.role === 'user' ? {kind:'user',text:shown} : {kind:'event',tone:'quiet',text:shown});
        } else if (entry.role === 'user' || entry.role === 'system') {
          // Mail read from a delivery is named as the stream names it, as stui does.
          fromHarness(entry.role === 'user', raw, shown, delivered, filters).forEach((part, index) => push(entry, `${entry.id}#${index}`, part.kind === 'mail' ? { ...part, from: name(part.from), to: name(part.to) } : part));
        } else if (entry.role === 'tool') {
          const lines = raw.split('\n');
          if (lines.join('').trim()) push(entry, entry.id, { kind: 'tool', title: lines[0], state: 'ok', output: lines.slice(1) });
        } else {
          const text = raw;
          if (text.trim()) push(entry, entry.id, entry.role === 'assistant' ? { kind: 'assistant', text } : {kind:'event',tone:'quiet',text:`[unknown role]\n${text}`});
        }
        break;
      }
      case 'tool_call': {
        const call = str(body.call_id);
        if (call) { tools.set(call, stamped.length); callSources.set(call, entry); }
        const view = blockView(body.blocks, 'tool_call');
        push(entry, entry.id, { kind: 'tool', title: view ? viewTitle(view, str(body.name) ?? 'tool', body.arguments) : toolTitle(str(body.name) ?? 'tool', body.arguments), state: 'running', output: view ? viewCallLines(view) : [] });
        break;
      }
      case 'tool_result': {
        const view = blockView(body.blocks, 'tool_output');
        const output = view ? viewOutput(view, body.content, fullOutput) ?? toolOutput(body.content, fullOutput) : toolOutput(body.content, fullOutput);
        const state: ToolState = body.status === 'error' || view?.is_error === true || view?.timed_out === true ? 'failed' : 'ok';
        const index = tools.get(str(body.call_id) ?? '');
        const call = index === undefined ? undefined : stamped[index];
        if (call?.body.kind === 'tool') {
          call.body = { ...call.body, state, output: view?.type === 'todo' ? output : [...markSelected(call.body.output, view), ...output] };
          const content = contentReferences(entry.body);
          if (content.length) {
            call.content = [...(call.content ?? []), ...content];
            const source = callSources.get(str(body.call_id) ?? '');
            call.contentSources = [...(call.contentSources ?? (source ? [source] : [])), entry];
          }
        }
        else push(entry, entry.id, { kind: 'tool', title: 'tool result', state, output });
        // Each finished subagent is its own card, opening its child conversation when it has one.
        if (view?.type === 'task' && Array.isArray(view.agents)) {
          for (const agent of view.agents) {
            const card = subagentCard(agent, state);
            push(entry, `${entry.id}#${card.id}`, { kind: 'tool', title: card.title, state, output: card.output });
          }
        }
        break;
      }
      case 'error': {
        if (str(body.code) === 'transcript-not-bound' && record(body.details).not_yet === true) {
          push(entry, entry.id, { kind: 'event', tone: 'quiet', text: 'nothing in the harness yet since this seat started' });
          break;
        }
        const tone = diagnosticTone(body);
        const message = str(body.message) ?? str(body.code) ?? 'st reported a problem';
        push(entry, entry.id, { kind: 'event', tone, text: tone === 'fault' ? `error: ${message}` : message });
        break;
      }
      case 'redaction': push(entry, entry.id, { kind: 'event', tone: 'quiet', text: `withheld: ${str(body.reason) ?? 'redacted'}` }); break;
      // Older entries were left out: it heads the conversation whatever time st stamped it with
      // (the time it was read, which sorted it among the newest).
      case 'truncation': stamped.push({ id: entry.id, at: '', timestamp: '', body: { kind: 'event', tone: 'quiet', text: (str(body.reason) ?? '').includes('native transcript prefix') ? NATIVE_PREFIX_NOTE + ((str(body.reason) ?? '').includes('not fetchable') ? '; earlier history is not fetchable through this read' : '') : (str(body.reason) ?? '').includes('not fetchable') ? 'older entries are not shown; not fetchable through this read' : 'older entries are not shown' } }); break;
      case 'status':
      case 'usage': break;
      default: {
        // A kind this app does not know yet: say that it happened.
        const text = contentText(entry.body) || str(body.message) || entry.type;
        push(entry, entry.id, { kind: 'event', tone: 'quiet', text: shorten(text, 120) });
      }
    }
  }
  for (const entry of stamped) {
    if (entry.body.kind === 'mail' && delivered.has(entry.id)) entry.body = { ...entry.body, delivered: true };
  }
  return foldDeliveryFlaps(stamped.map((entry, order) => ({ entry, order })).sort((a, b) => a.entry.timestamp.localeCompare(b.entry.timestamp) || a.order - b.order).map(({ entry }) => entry));
}

/** Whether an entry says `query` anywhere a person reads, case aside: for finding in a conversation. */
function said(entry: ConversationEntry): string[] {
  const body = entry.body;
  return body.kind === 'tool' ? [body.title, ...body.output]
    : body.kind === 'mail' ? [body.from, body.to, body.subject, body.text]
    : [body.text];
}

export function entryMatches(entry: ConversationEntry, query: string): boolean {
  const wanted = query.trim().toLowerCase();
  if (!wanted) return true;
  return said(entry).some(text => text.toLowerCase().includes(wanted));
}

/** An entry as plain text, for selecting and copying. */
export function entryText(entry: ConversationEntry): string {
  const body = entry.body;
  if (body.kind === 'tool') return [body.title, ...body.output].join('\n');
  if (body.kind === 'mail') return [`${body.to ? `${body.from} → ${body.to}` : body.from}${body.subject ? `  ${body.subject}` : ''}`, '', body.text].join('\n');
  return body.text;
}

/**
 * A delivery that paused while st restarted and then recovered says nothing a person needs:
 * each such pair becomes one quiet line, and a run of them one line with a count.
 */
export function foldDeliveryFlaps(entries: ConversationEntry[]): ConversationEntry[] {
  const out: ConversationEntry[] = [];
  const isFlap = (entry: ConversationEntry | undefined) => entry?.body.kind === 'event' && /^Native conversation delivery over \S+ (paused|recovered|failed)/.test(entry.body.text);
  for (let index = 0; index < entries.length;) {
    if (!isFlap(entries[index])) { out.push(entries[index++]); continue; }
    const start = index;
    while (index < entries.length && isFlap(entries[index])) index++;
    const run = entries.slice(start, index);
    const last = run.at(-1)!;
    const recovered = last.body.kind === 'event' && last.body.text.includes('recovered');
    const pauses = run.filter(entry => entry.body.kind === 'event' && !entry.body.text.includes('recovered')).length;
    out.push({ ...last, id: `${run[0].id}…${last.id}`, body: { kind: 'event', tone: recovered ? 'quiet' : 'warning', text: recovered ? `message delivery paused ${pauses === 1 ? 'once' : `${pauses} times`} while st restarted · recovered` : 'message delivery is paused; st will retry' } });
  }
  return out;
}

/** The most lines a tool call shows until opened, failed ones too, as in stui. */
/** As in stui's conversation-style rules (`tool.collapsed_rows`); a test holds them equal. */
export const COLLAPSED_TOOL_LINES = 6;

/** Whether an entry folds until opened: tool calls, and mail the person is not part of. */
export function folds(body: Body): boolean {
  return body.kind === 'tool' || (body.kind === 'mail' && body.from !== 'you' && body.to !== 'you');
}
/** The lines a tool box shows: all when open, failed, or short; otherwise its last five. */
export function shownToolLines(body: Extract<Body, { kind: 'tool' }>, open: boolean): { hidden: number; lines: string[] } {
  if (!open && body.title.startsWith('assistant error · recovered')) return { hidden: body.output.length, lines: [] };
  if (open || body.output.length <= COLLAPSED_TOOL_LINES) return { hidden: 0, lines: body.output };
  const head = body.title.startsWith('write ') || body.title.startsWith('compaction');
  return { hidden: body.output.length - COLLAPSED_TOOL_LINES, lines: head ? body.output.slice(0, COLLAPSED_TOOL_LINES) : body.output.slice(-COLLAPSED_TOOL_LINES) };
}

/** What a problem means for what is shown: how old it is and that the phone keeps trying. */
export function staleLine(issue: string, loaded: boolean, lastFrame: number | null, now: number): string {
  if (!loaded) return `Not loaded yet: ${issue}. Trying again.`;
  if (lastFrame === null) return `${issue} · trying again`;
  const seconds = Math.max(0, Math.round((now - lastFrame) / 1000));
  const age = seconds < 60 ? `${seconds}s` : seconds < 3600 ? `${Math.floor(seconds / 60)}m` : `${Math.floor(seconds / 3600)}h`;
  return `${issue} · shown as of ${age} ago · trying again`;
}

const hasClippedValue = (value: unknown): boolean => typeof value === 'string'
  ? value.includes('[st truncated this native timeline value')
  : Array.isArray(value) ? value.some(hasClippedValue)
  : value !== null && typeof value === 'object' ? Object.values(value).some(hasClippedValue) : false;

/** Use the normal typed projection for an identifiable full body, a typed view
 * continuation, or an unambiguous clipped payload; unknown metadata stays raw. */
export const fetchedConversationEntries = (
  entry: ConversationEntry, reference: ConversationContentRef, value: unknown,
): ConversationEntry[] | undefined => {
  const source = entry.contentSources?.find(source => {
    const blocks = record(source.body).blocks;
    return Array.isArray(blocks) && blocks.some(block => record(record(block).continuation).ref === reference.ref);
  });
  if (!source) return undefined;
  const original = record(source.body);
  const block = Array.isArray(original.blocks)
    ? original.blocks.map(record).find(block => record(block.continuation).ref === reference.ref) : undefined;
  if (!block) return undefined;
  const payload = hasClippedValue(block.payload) && !hasClippedValue(block.metadata) && !hasClippedValue(block.view);
  const full = record(value);
  let body: unknown;
  const sourceBlock = record(block);
  const fetchedView = !hasClippedValue(value) && typeof full.type === 'string' && full.type === record(sourceBlock.view).type && hasClippedValue(sourceBlock.view);
  if (fetchedView && !hasClippedValue(sourceBlock.metadata) && !hasClippedValue(sourceBlock.payload)) {
    const blocks = Array.isArray(original.blocks) ? original.blocks.map(candidate => candidate === block ? { ...sourceBlock, view: value } : candidate) : [];
    body = { ...original, blocks };
  }
  else if (source.type === 'tool_result') {
    if (full.call_id === original.call_id && typeof full.media_type === 'string'
      && ['success', 'error', 'unknown'].includes(String(full.status)) && 'content' in full) body = full;
    else if (payload) body = { ...original, content: value };
    else return undefined;
  } else if (source.type === 'tool_call') {
    if (full.call_id === original.call_id && full.name === original.name && 'arguments' in full) body = full;
    else if (payload) body = { ...original, arguments: value };
    else return undefined;
  } else if (source.type === 'content' && (['text', 'reasoning'].includes(String(block.kind)) || record(block.view).type === 'compaction')) {
    if (typeof full.media_type === 'string' && typeof full.text === 'string') body = full;
    else if (payload && typeof value === 'string') body = { ...original, text: value };
    else return undefined;
  } else return undefined;
  const call = source.type === 'tool_result' ? entry.contentSources?.find(candidate => candidate.type === 'tool_call' && record(candidate.body).call_id === original.call_id) : undefined;
  return conversationEntries([...(call ? [call] : []), { ...source, body }], new Map(), DEFAULT_FILTERS, true).map(shown => {
    const body = shown.body.kind === 'tool' && entry.body.kind === 'tool'
      ? { ...shown.body, title: entry.body.title } : shown.body;
    return { id: shown.id, at: shown.at, timestamp: shown.timestamp, body };
  });
};
