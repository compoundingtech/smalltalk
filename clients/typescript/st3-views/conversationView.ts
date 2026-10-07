import type { TimelineEntry } from '@smalltalk/st3-client';

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
export type DisplayFilter = 'harness-markup' | 'context-blocks' | 'control-characters' | 'internal-blocks' | 'excerpts';
export const DEFAULT_FILTERS: readonly DisplayFilter[] = ['harness-markup', 'context-blocks', 'control-characters', 'internal-blocks', 'excerpts'];
export const SHOW_EVERYTHING: readonly DisplayFilter[] = [];

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

export function conversationEntries(timeline: Entry[], names: Names, filters: readonly DisplayFilter[] = DEFAULT_FILTERS): ConversationEntry[] {
  if (!filters.length) return timeline.map(entry => ({id: entry.id, at: clock(entry.timestamp), timestamp: entry.timestamp, body: {kind: 'user', text: JSON.stringify(entry, null, 2)}}));
  timeline = timeline.map(entry => {
    if (entry.body === null || typeof entry.body !== 'object' || Array.isArray(entry.body)) return entry;
    const body = {...record(entry.body)};
    if (filters.includes('internal-blocks') && Array.isArray(body.blocks)) body.blocks = body.blocks.filter(block => !['internal', 'hidden-by-harness'].includes(str(record(block).visibility) ?? ''));
    if (typeof body.text === 'string') {
      const text = {value: body.text};
      if (filters.includes('context-blocks') && !['user','system'].includes(entry.role)) filterContextBlocks(text);
      if (!['user','system'].includes(entry.role)) text.value = cleanMessageText(text.value, filters);
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
  const shown = new Set(timeline.filter(entry => entry.type === 'message').map(entry => str(record(entry.body).message_id) ?? '').filter(id => id.startsWith('message/')));
  const delivered = new Set<string>();
  let mail: Record<string, unknown> | undefined;
  const push = (entry: Entry, id: string, body: Body) => stamped.push({ id, at: clock(entry.timestamp), timestamp: entry.timestamp, body });
  // Entries arrive in st's order (applyConversation keeps them by time, then sequence).
  for (const entry of timeline) {
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
      continue;
    }
    mail = undefined;
    switch (entry.type) {
      case 'content': {
        const raw = contentText(entry.body);
        const nativeBlocks = Array.isArray(body.blocks) && body.blocks.some(block => record(block).kind !== 'text');
        if ((entry.role === 'user' || entry.role === 'system') && (nativeBlocks || raw.startsWith('[unrecognized '))) {
          push(entry, entry.id, entry.role === 'user' ? {kind:'user',text:cleanMessageText(raw,filters)} : {kind:'event',tone:'quiet',text:cleanMessageText(raw,filters)});
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
      // Older entries were left out: it heads the conversation whatever time st stamped it with
      // (the time it was read, which sorted it among the newest).
      case 'truncation': stamped.push({ id: entry.id, at: '', timestamp: '', body: { kind: 'event', tone: 'quiet', text: (str(body.reason) ?? '').includes('not fetchable') ? 'older entries are not shown; not fetchable through this read' : 'older entries are not shown' } }); break;
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
  if (open || body.output.length <= COLLAPSED_TOOL_LINES) return { hidden: 0, lines: body.output };
  return { hidden: body.output.length - COLLAPSED_TOOL_LINES, lines: body.output.slice(-COLLAPSED_TOOL_LINES) };
}

/** What a problem means for what is shown: how old it is and that the phone keeps trying. */
export function staleLine(issue: string, loaded: boolean, lastFrame: number | null, now: number): string {
  if (!loaded) return `Not loaded yet: ${issue}. Trying again.`;
  if (lastFrame === null) return `${issue} · trying again`;
  const seconds = Math.max(0, Math.round((now - lastFrame) / 1000));
  const age = seconds < 60 ? `${seconds}s` : seconds < 3600 ? `${Math.floor(seconds / 60)}m` : `${Math.floor(seconds / 3600)}h`;
  return `${issue} · shown as of ${age} ago · trying again`;
}
