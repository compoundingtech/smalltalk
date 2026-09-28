// One conversation: an agent's harness transcript and its Small Talk messages in time order,
// with harness markup turned into what it means. The port of stui's `adapt::conversation`;
// fixtures/clients/transcripts/*.expected.json pin the result.

import type { Body, Entry } from './clientView';
import { cleanMessageText, lines, trim } from './messageText.ts';

/** The timeline and message fields the cleaning reads; the st3 client types satisfy these. */
export type TimelineLike = {
  id: string;
  timestamp: string;
  role: 'system' | 'user' | 'assistant' | 'tool';
  type: string;
  body: unknown;
};
export type MessageLike = {
  id: string;
  from: string;
  to: string;
  title?: string | null;
  content: string;
  sent_at: string;
};

/** Blocks harnesses add to a transcript for the model's benefit. None of it is conversation. */
const CONTEXT_BLOCKS = [
  'system-reminder', 'local-command-caveat', 'environment_context', 'permissions', 'collaboration_mode', 'multi_agent_mode',
  'apps_instructions', 'plugins_instructions', 'skills_instructions', 'user_instructions', 'developer_instructions',
  'command-message', 'command-args',
];

/** Strip repeated prefixes, as Rust's `trim_start_matches` does. */
export function trimPrefix(text: string, prefix: string): string {
  let out = text;
  while (prefix && out.startsWith(prefix)) out = out.slice(prefix.length);
  return out;
}
export const short = (id: string) => trimPrefix(trimPrefix(id, 'mission/'), 'agent/');

type Holder = { text: string };

/** Take every `<tag …>…</tag>` block out of `holder.text`, returning the inner texts. A block
 * that never closes runs to the end, so a truncated wrapper cannot leak either. */
function takeBlocks(holder: Holder, tag: string): string[] {
  const found: string[] = [];
  const open = `<${tag}`;
  const close = `</${tag}>`;
  let from = 0;
  for (;;) {
    const start = holder.text.indexOf(open, from);
    if (start === -1) break;
    const afterStart = start + open.length;
    // `<permissions` must not match `<permissionsfoo`.
    if (!['>', ' ', '/', '\n'].includes(holder.text[afterStart] ?? '')) {
      from = afterStart;
      continue;
    }
    const headOffset = holder.text.indexOf('>', afterStart);
    if (headOffset === -1) {
      holder.text = holder.text.slice(0, start);
      break;
    }
    const headEnd = headOffset + 1;
    const closeAt = holder.text.indexOf(close, headEnd);
    const [inner, end] = closeAt === -1
      ? [holder.text.slice(headEnd), holder.text.length]
      : [holder.text.slice(headEnd, closeAt), closeAt + close.length];
    found.push(inner);
    holder.text = holder.text.slice(0, start) + holder.text.slice(end);
    from = start;
  }
  return found;
}

function field(block: string, tag: string): string | undefined {
  const value = takeBlocks({ text: block }, tag)[0];
  return value === undefined ? undefined : trim(value);
}

function shorten(text: string, max: number): string {
  const line = trim(lines(text)[0] ?? '');
  const characters = Array.from(line);
  return characters.length <= max ? line : `${characters.slice(0, max).join('')}…`;
}

/** A user or system entry from a harness transcript, turned into what a person should see. */
export function fromHarness(isUser: boolean, raw: string): Body[] {
  const holder = { text: raw.replaceAll('\r\n', '\n') };
  const bodies: Body[] = [];
  for (const block of takeBlocks(holder, 'task-notification')) {
    const status = field(block, 'status') ?? 'update';
    const summary = field(block, 'summary') ?? '';
    bodies.push({ kind: 'event', value: `background task ${status}: ${shorten(summary, 90)}` });
  }
  // st deliveries: the message itself is in the stream as mail.
  const senders: string[] = [];
  for (let from = 0; ;) {
    const start = holder.text.indexOf('<channel', from);
    if (start === -1) break;
    const headOffset = holder.text.indexOf('>', start);
    const head = holder.text.slice(start, headOffset === -1 ? holder.text.length : headOffset + 1);
    const rest = head.split('from="')[1];
    senders.push(rest === undefined ? 'someone' : rest.split('"')[0]);
    from = start + 1;
  }
  takeBlocks(holder, 'channel').forEach((block, index) => {
    if (index >= senders.length) return;
    const subjectLine = lines(block).map(line => trim(line)).find(line => line.startsWith('Subject:'));
    const subject = subjectLine !== undefined ? trim(subjectLine.slice('Subject:'.length)) : shorten(cleanMessageText(block), 70);
    bodies.push({ kind: 'event', value: `delivered to the agent: ${shorten(subject, 80)} · from ${senders[index]}` });
  });
  for (const command of takeBlocks(holder, 'command-name')) {
    const args = field(raw, 'command-args') ?? '';
    bodies.push({ kind: 'user', value: trim(`${trim(command)} ${args}`) });
  }
  for (const [tag, state] of [['local-command-stdout', 'ok'], ['local-command-stderr', 'failed']] as const) {
    for (const output of takeBlocks(holder, tag)) {
      bodies.push({ kind: 'tool', value: { title: 'command output', state, output: lines(trim(output)) } });
    }
  }
  for (const _ of takeBlocks(holder, 'turn_aborted')) bodies.push({ kind: 'event', value: 'the turn was interrupted' });
  for (const reply of takeBlocks(holder, 'send_user_message_question_reply')) {
    let parsed: unknown;
    try { parsed = JSON.parse(trim(reply)); } catch { parsed = undefined; }
    const answers = (Array.isArray(parsed) ? parsed : [])
      .map(item => (item && typeof item === 'object' ? (item as { answer?: unknown }).answer : undefined))
      .filter((answer): answer is string => typeof answer === 'string');
    if (answers.length) bodies.push({ kind: 'user', value: answers.join('\n') });
  }
  for (const tag of CONTEXT_BLOCKS) takeBlocks(holder, tag);
  const rest = cleanMessageText(holder.text);
  if (isUser && rest !== '') bodies.unshift({ kind: 'user', value: rest });
  return bodies;
}

/** A local display time, like "09:05". */
export function clock(timestamp: string): string {
  const time = new Date(timestamp);
  if (!/^\d{4}-\d{2}-\d{2}T/.test(timestamp) || Number.isNaN(time.getTime())) return '';
  return `${String(time.getHours()).padStart(2, '0')}:${String(time.getMinutes()).padStart(2, '0')}`;
}

const get = (value: unknown, key: string): unknown => (value && typeof value === 'object' && !Array.isArray(value) ? (value as Record<string, unknown>)[key] : undefined);

export function toolTitle(name: string, args: unknown): string {
  const pick = ['command', 'cmd', 'file_path', 'path', 'pattern', 'url', 'query', 'description']
    .map(key => get(args, key)).find((value): value is string => typeof value === 'string');
  if (pick === undefined) return name;
  const first = lines(pick)[0] ?? pick;
  return ['Bash', 'bash', 'shell', 'exec_command'].includes(name) ? `$ ${first}` : `${name} ${first}`;
}

/** serde_json's compact form: object keys sorted. */
function compactJson(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(compactJson).join(',')}]`;
  if (value && typeof value === 'object') {
    return `{${Object.keys(value).sort().map(key => `${JSON.stringify(key)}:${compactJson((value as Record<string, unknown>)[key])}`).join(',')}}`;
  }
  return JSON.stringify(value) ?? 'null';
}

export function toolOutput(content: unknown): string[] {
  let text: string;
  if (typeof content === 'string') text = content;
  else if (Array.isArray(content)) {
    text = content.map(item => { const inner = get(item, 'text'); return typeof inner === 'string' ? inner : typeof item === 'string' ? item : undefined; })
      .filter((item): item is string => item !== undefined).join('\n');
  } else if (content === null || content === undefined) text = '';
  else { const inner = get(content, 'text'); text = typeof inner === 'string' ? inner : compactJson(content); }
  return lines(text).slice(0, 400);
}

/** Display names for message headers: "you", "st", agent names. */
export function nameFor(names: Record<string, string>, id: string): string {
  if (Object.hasOwn(names, id)) return names[id];
  if (id === 'daemon/runtime') return 'st';
  if (id.startsWith('person/')) return trimPrefix(id, 'person/');
  return short(id);
}

export function conversation(timeline: TimelineLike[], messages: MessageLike[], names: Record<string, string>): Entry[] {
  const stamped: [string, Entry][] = [];
  const tools = new Map<string, number>();
  for (const entry of timeline) {
    const at = clock(entry.timestamp);
    const body = entry.body;
    let out: Body | undefined;
    if (entry.type === 'content') {
      const raw = typeof get(body, 'text') === 'string' ? get(body, 'text') as string : '';
      if (entry.role === 'user' || entry.role === 'system') {
        fromHarness(entry.role === 'user', raw).forEach((harnessBody, index) => {
          stamped.push([entry.timestamp, { id: `${entry.id}#${index}`, at, body: harnessBody }]);
        });
        continue;
      }
      const text = cleanMessageText(raw);
      if (text === '') continue;
      if (entry.role === 'assistant') out = { kind: 'assistant', value: text };
      else {
        const [title = '', ...output] = lines(text);
        out = { kind: 'tool', value: { title, state: 'ok', output } };
      }
    } else if (entry.type === 'tool_call') {
      tools.set(String(get(body, 'call_id')), stamped.length);
      out = { kind: 'tool', value: { title: toolTitle(String(get(body, 'name') ?? ''), get(body, 'arguments')), state: 'running', output: [] } };
    } else if (entry.type === 'tool_result') {
      const output = toolOutput(get(body, 'content'));
      const state = get(body, 'status') === 'error' ? 'failed' : 'ok';
      const index = tools.get(String(get(body, 'call_id')));
      const slot = index === undefined ? undefined : stamped[index]?.[1];
      if (slot && slot.body.kind === 'tool') {
        slot.body = { kind: 'tool', value: { ...slot.body.value, state, output } };
        continue;
      }
      out = { kind: 'tool', value: { title: 'tool result', state, output } };
    } else if (entry.type === 'error') {
      out = { kind: 'event', value: `error: ${String(get(body, 'message') ?? '')}` };
    }
    if (out) stamped.push([entry.timestamp, { id: entry.id, at, body: out }]);
  }
  for (const message of messages) {
    const text = cleanMessageText(message.content);
    if (message.from === 'daemon/runtime') {
      // Step-ready pings are graph events, not conversation.
      stamped.push([message.sent_at, { id: message.id, at: clock(message.sent_at), body: { kind: 'event', value: message.title ?? lines(text)[0] ?? '' } }]);
      continue;
    }
    stamped.push([message.sent_at, {
      id: message.id,
      at: clock(message.sent_at),
      body: { kind: 'mail', value: { from: nameFor(names, message.from), to: nameFor(names, message.to), subject: message.title ?? '', body: text === '' ? '(notification)' : text } },
    }]);
  }
  stamped.sort((a, b) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0));
  return stamped.map(([, entry]) => entry);
}
