import type { TimelineEntry } from '../../clients/typescript/st3-client';

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
  | { kind: 'mail'; from: string; to: string; subject: string; text: string }
  | { kind: 'event'; text: string; tone: 'quiet' | 'warning' | 'fault' };
export type ConversationEntry = { id: string; at: string; timestamp: string; body: Body };

type Entry = Pick<TimelineEntry, 'id' | 'role' | 'timestamp'> & { type: string; body: unknown };
type Names = ReadonlyMap<string, string>;

function record(value: unknown): Record<string, unknown> {
  return value && typeof value === 'object' && !Array.isArray(value) ? value as Record<string, unknown> : {};
}
function str(value: unknown): string | undefined {
  return typeof value === 'string' ? value : undefined;
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

function stripInternalMarkup(input: string): string {
  let inChannel = false;
  let text = input.split('\n').filter(line => {
    const trimmed = line.trim();
    if (trimmed.startsWith('<channel ') && trimmed.includes('source="plugin:st3-channel:st3"') && trimmed.endsWith('>')) { inChannel = true; return false; }
    if (inChannel && trimmed === '</channel>') { inChannel = false; return false; }
    return !(trimmed.startsWith('[st3-delivery:') && trimmed.endsWith('.md]'));
  }).join('\n');
  for (const tag of HIDDEN_TAGS) {
    for (;;) {
      const lower = text.toLowerCase();
      const start = lower.indexOf(`<${tag}`);
      if (start < 0) break;
      const openEnd = lower.indexOf('>', start);
      if (openEnd < 0) { text = text.slice(0, start); break; }
      const close = lower.indexOf(`</${tag}>`, openEnd + 1);
      if (close < 0) { text = text.slice(0, start); break; }
      text = text.slice(0, start) + text.slice(close + tag.length + 3);
    }
  }
  return text;
}

/** What a message says once the markup harnesses and st add for the model is gone. */
export function cleanMessageText(raw: string): string {
  const normalized = raw.replace(/\r\n/g, '\n');
  let output = '', plain = '', code = false;
  for (const line of normalized.split(/(?<=\n)/)) {
    if (line.trimStart().startsWith('```')) {
      if (!code) { output += stripInternalMarkup(plain); plain = ''; }
      output += line;
      code = !code;
    } else if (code) output += line;
    else plain += line;
  }
  output += stripInternalMarkup(plain);
  // eslint-disable-next-line no-control-regex
  const safe = output.replace(/[\u0000-\u0008\u000b-\u001f\u007f]/g, '').trim();
  const text = (safe.startsWith('[PING] ?') ? safe.slice('[PING] ?'.length) : safe).trim();
  const reference = text.lastIndexOf(' [id:message/');
  return reference >= 0 && text.endsWith(']') ? text.slice(0, reference).trimEnd() : text;
}

const CONTEXT_BLOCKS = ['system-reminder', 'local-command-caveat', 'environment_context', 'permissions', 'collaboration_mode', 'multi_agent_mode', 'apps_instructions', 'plugins_instructions', 'skills_instructions', 'user_instructions', 'developer_instructions', 'command-message', 'command-args'];

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
function field(block: string, tag: string): string | undefined {
  return takeBlocks({ value: block }, tag)[0]?.trim();
}
function shorten(text: string, max: number): string {
  const line = (text.split('\n')[0] ?? '').trim();
  return [...line].length <= max ? line : `${[...line].slice(0, max).join('')}…`;
}

/** A user or system entry from a harness transcript, turned into what a person should see. */
export function fromHarness(isUser: boolean, raw: string): Body[] {
  const text = { value: raw.replace(/\r\n/g, '\n') };
  const bodies: Body[] = [];
  for (const block of takeBlocks(text, 'task-notification')) {
    bodies.push({ kind: 'event', tone: 'quiet', text: `background task ${field(block, 'status') ?? 'update'}: ${shorten(field(block, 'summary') ?? '', 90)}` });
  }
  const senders = [...text.value.matchAll(/<channel\b[^>]*>/g)].map(match => /from="([^"]*)"/.exec(match[0])?.[1] ?? 'someone');
  takeBlocks(text, 'channel').forEach((block, index) => {
    const subject = block.split('\n').map(line => line.trim()).find(line => line.startsWith('Subject:'))?.slice('Subject:'.length).trim() ?? shorten(cleanMessageText(block), 70);
    bodies.push({ kind: 'event', tone: 'quiet', text: `delivered to the agent: ${shorten(subject, 80)} · from ${senders[index] ?? 'someone'}` });
  });
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
  for (const tag of CONTEXT_BLOCKS) takeBlocks(text, tag);
  const rest = cleanMessageText(text.value);
  if (isUser && rest) bodies.unshift({ kind: 'user', text: rest });
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

export function toolOutput(content: unknown): string[] {
  let text: string;
  if (typeof content === 'string') text = content;
  else if (Array.isArray(content)) text = content.map(item => str(record(item).text) ?? str(item)).filter((part): part is string => part !== undefined).join('\n');
  else if (content == null) text = '';
  else text = str(record(content).text) ?? JSON.stringify(content);
  return text ? text.split('\n').slice(0, 400) : [];
}

function contentText(body: unknown): string {
  if (typeof body === 'string') return body;
  return str(record(body).text) ?? '';
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
export function conversationEntries(timeline: Entry[], names: Names): ConversationEntry[] {
  const name = (id: string): string => names.get(id) ?? (id === 'daemon/runtime' ? 'st' : id.startsWith('person/') ? id.slice('person/'.length) : short(id));
  const stamped: ConversationEntry[] = [];
  const tools = new Map<string, number>();
  let mail: Record<string, unknown> | undefined;
  const push = (entry: Entry, id: string, body: Body) => stamped.push({ id, at: clock(entry.timestamp), timestamp: entry.timestamp, body });
  // Entries arrive in st's order (applyConversation keeps them by sequence).
  for (const entry of timeline) {
    const body = record(entry.body);
    if (entry.type === 'message') {
      // A Small Talk message is two entries: who wrote to whom, then what they wrote. Only a
      // graph message, `message/…`, is Small Talk; a transcript heads its own turns this way too.
      mail = (str(body.message_id) ?? '').startsWith('message/') ? body : undefined;
      continue;
    }
    if (mail && entry.type === 'content') {
      const message = mail;
      mail = undefined;
      const text = cleanMessageText(contentText(entry.body));
      const from = str(message.from) ?? '';
      if (from === 'daemon/runtime') {
        // Step-ready pings are graph events, not conversation.
        push(entry, str(message.message_id)!, { kind: 'event', tone: 'quiet', text: str(message.title) ?? text.split('\n')[0] ?? '' });
      } else {
        const to = str(message.to);
        push(entry, str(message.message_id)!, {
          kind: 'mail',
          from: !from && !to ? 'Small Talk' : name(from),
          to: !from && !to ? '' : name(to ?? ''),
          subject: str(message.title) ?? '',
          text: text || '(notification)',
        });
      }
      continue;
    }
    mail = undefined;
    switch (entry.type) {
      case 'content': {
        const raw = contentText(entry.body);
        if (entry.role === 'user' || entry.role === 'system') {
          fromHarness(entry.role === 'user', raw).forEach((part, index) => push(entry, `${entry.id}#${index}`, part));
        } else if (entry.role === 'tool') {
          const lines = cleanMessageText(raw).split('\n');
          if (lines.join('').trim()) push(entry, entry.id, { kind: 'tool', title: lines[0], state: 'ok', output: lines.slice(1) });
        } else {
          const text = cleanMessageText(raw);
          if (text) push(entry, entry.id, { kind: 'assistant', text });
        }
        break;
      }
      case 'tool_call': {
        const call = str(body.call_id);
        if (call) tools.set(call, stamped.length);
        push(entry, entry.id, { kind: 'tool', title: toolTitle(str(body.name) ?? 'tool', body.arguments), state: 'running', output: [] });
        break;
      }
      case 'tool_result': {
        const output = toolOutput(body.content);
        const state: ToolState = body.status === 'error' ? 'failed' : 'ok';
        const index = tools.get(str(body.call_id) ?? '');
        const call = index === undefined ? undefined : stamped[index];
        if (call?.body.kind === 'tool') { call.body = { ...call.body, state, output }; break; }
        push(entry, entry.id, { kind: 'tool', title: 'tool result', state, output });
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
      case 'truncation': push(entry, entry.id, { kind: 'event', tone: 'quiet', text: 'older entries are not shown' }); break;
      case 'status':
      case 'usage': break;
      default: {
        // A kind this app does not know yet: say that it happened.
        const text = contentText(entry.body) || str(body.message) || entry.type;
        push(entry, entry.id, { kind: 'event', tone: 'quiet', text: shorten(text, 120) });
      }
    }
  }
  return foldDeliveryFlaps(stamped.map((entry, order) => ({ entry, order })).sort((a, b) => a.entry.timestamp.localeCompare(b.entry.timestamp) || a.order - b.order).map(({ entry }) => entry));
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

export const COLLAPSED_TOOL_LINES = 5;
/** The lines a tool box shows: all when open, failed, or short; otherwise its last five. */
export function shownToolLines(body: Extract<Body, { kind: 'tool' }>, open: boolean): { hidden: number; lines: string[] } {
  if (open || body.state === 'failed' || body.output.length <= COLLAPSED_TOOL_LINES) return { hidden: 0, lines: body.output };
  return { hidden: body.output.length - COLLAPSED_TOOL_LINES, lines: body.output.slice(-COLLAPSED_TOOL_LINES) };
}
