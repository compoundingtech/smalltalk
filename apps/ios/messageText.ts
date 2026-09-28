// Message and transcript text as a person should read it: the port of stui's
// `clean_message_text`. Model-only markup disappears; fenced code is left exactly as written.
// String helpers here follow Rust's `str` semantics so both clients clean text identically.

const RUST_WHITESPACE = new Set(['\t', '\n', '\v', '\f', '\r', ' ', '\u0085', ' ', ' ', ' ', ' ', ' ', ' ', ' ', ' ', ' ', ' ', ' ', ' ', ' ', ' ', ' ', ' ', ' ', '　']);

export function trimStart(text: string): string {
  let start = 0;
  while (start < text.length && RUST_WHITESPACE.has(text[start])) start++;
  return text.slice(start);
}
export function trimEnd(text: string): string {
  let end = text.length;
  while (end > 0 && RUST_WHITESPACE.has(text[end - 1])) end--;
  return text.slice(0, end);
}
export const trim = (text: string) => trimEnd(trimStart(text));

/** `str::lines`: split on `\n`, drop one trailing `\r` per line, no empty line after a final `\n`. */
export function lines(text: string): string[] {
  if (text === '') return [];
  const parts = text.split('\n');
  if (parts[parts.length - 1] === '') parts.pop();
  return parts.map(line => (line.endsWith('\r') ? line.slice(0, -1) : line));
}

function splitInclusive(text: string): string[] {
  const parts: string[] = [];
  let from = 0;
  while (from < text.length) {
    const next = text.indexOf('\n', from);
    const end = next === -1 ? text.length : next + 1;
    parts.push(text.slice(from, end));
    from = end;
  }
  return parts;
}

/** ASCII-only lowercase, so offsets in the result match offsets in the input. */
export const asciiLower = (text: string) => text.replace(/[A-Z]/g, letter => letter.toLowerCase());

const CONTROL = /\p{Cc}/u;

export function cleanMessageText(raw: string): string {
  const normalized = raw.replaceAll('\r\n', '\n');
  let output = '';
  let plain = '';
  let code = false;
  for (const line of splitInclusive(normalized)) {
    if (trimStart(line).startsWith('```')) {
      if (!code) {
        output += stripInternalMarkup(plain);
        plain = '';
      }
      output += line;
      code = !code;
    } else if (code) {
      output += line;
    } else {
      plain += line;
    }
  }
  output += stripInternalMarkup(plain);
  const safe = Array.from(output).filter(character => character === '\n' || character === '\t' || !CONTROL.test(character)).join('');
  const trimmed = trim(safe);
  const text = trim(trimmed.startsWith('[PING] ?') ? trimmed.slice('[PING] ?'.length) : trimmed);
  const start = text.lastIndexOf(' [id:message/');
  if (start !== -1 && text.endsWith(']')) return trimEnd(text.slice(0, start));
  return text;
}

function stripInternalMarkup(input: string): string {
  let inChannel = false;
  let text = lines(input).filter(line => {
    const trimmed = trim(line);
    if (trimmed.startsWith('<channel ') && trimmed.includes('source="plugin:st3-channel:st3"') && trimmed.endsWith('>')) {
      inChannel = true;
      return false;
    }
    if (inChannel && trimmed === '</channel>') {
      inChannel = false;
      return false;
    }
    if (trimmed.startsWith('[st3-delivery:') && trimmed.endsWith('.md]')) return false;
    return true;
  }).join('\n');
  if (input.endsWith('\n') && text !== '') text += '\n';
  for (const tag of ['analysis', 'thinking', 'think', 'internal', 'system-reminder', 'function_calls', 'tool_result']) {
    for (;;) {
      const lower = asciiLower(text);
      const start = lower.indexOf(`<${tag}`);
      if (start === -1) break;
      const openOffset = lower.indexOf('>', start);
      if (openOffset === -1) { text = text.slice(0, start); break; }
      const openEnd = openOffset + 1;
      const close = lower.indexOf(`</${tag}>`, openEnd);
      if (close === -1) { text = text.slice(0, start); break; }
      text = text.slice(0, start) + text.slice(close + tag.length + 3);
    }
  }
  for (;;) {
    const start = text.indexOf('<|im_start|>');
    if (start === -1) break;
    const separator = text.indexOf('<|im_sep|>', start);
    if (separator === -1) { text = text.slice(0, start); break; }
    const header = text.slice(start, separator);
    const hidden = header.includes('<|meta_sep|>analysis') || header.includes('<|meta_sep|>commentary');
    const bodyStart = separator + '<|im_sep|>'.length;
    if (hidden) {
      const endOffset = text.indexOf('<|im_end|>', bodyStart);
      const end = endOffset === -1 ? text.length : endOffset + '<|im_end|>'.length;
      text = text.slice(0, start) + text.slice(end);
    } else {
      text = text.slice(0, start) + text.slice(bodyStart);
    }
  }
  for (const token of ['<|im_end|>', '<|fim_suffix|>', '<|im_sep|>']) text = text.replaceAll(token, '');
  return text;
}
