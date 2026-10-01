import type { TerminalModes } from '../../clients/typescript/st3-client/Models.generated';

// What the phone's keyboard and key bar send to a terminal, as the bytes a terminal emulator
// would send for the same keys, so full-screen programs such as vim work.

export type TerminalKey = 'escape' | 'tab' | 'enter' | 'backspace' | 'up' | 'down' | 'left' | 'right' | 'home' | 'end' | 'pageup' | 'pagedown';

type KeyModes = Pick<TerminalModes, 'application_cursor'>;

const ARROWS: Partial<Record<TerminalKey, string>> = { up: 'A', down: 'B', right: 'C', left: 'D', home: 'H', end: 'F' };

export function keyBytes(key: TerminalKey, modes?: KeyModes): string {
  const arrow = ARROWS[key];
  if (arrow) return `\x1b${modes?.application_cursor ? 'O' : '['}${arrow}`;
  switch (key) {
    case 'escape': return '\x1b';
    case 'tab': return '\t';
    case 'enter': return '\r';
    case 'backspace': return '\x7f';
    case 'pageup': return '\x1b[5~';
    case 'pagedown': return '\x1b[6~';
    default: return '';
  }
}

/** Ctrl with a character: Ctrl+A is 0x01 … Ctrl+Z 0x1a, Ctrl+[ is Esc, Ctrl+Space is NUL. */
export function controlBytes(text: string): string {
  const [first = '', ...rest] = Array.from(text);
  const code = first.toUpperCase().charCodeAt(0);
  const control = first === ' ' ? '\0'
    : code >= 0x40 && code <= 0x5f ? String.fromCharCode(code & 0x1f)
    : first === '?' ? '\x7f'
    : first;
  return control + rest.join('');
}

/** iOS replaces quotes and dashes as people type; a terminal wants what was typed. */
export function plainTyping(text: string): string {
  return text.replace(/[‘’]/g, "'").replace(/[“”]/g, '"').replace(/—/g, '--').replace(/…/g, '...');
}

/** A mouse wheel turn at a cell (zero-based), for a program that asked for mouse reports. */
export function wheelBytes(up: boolean, column: number, row: number, modes: Pick<TerminalModes, 'mouse_tracking' | 'mouse_encoding'>): string {
  if (modes.mouse_tracking === 'none') return '';
  const button = up ? 64 : 65;
  if (modes.mouse_encoding === 'sgr') return `\x1b[<${button};${column + 1};${row + 1}M`;
  const cell = (value: number) => String.fromCharCode(Math.min(32 + value + 1, modes.mouse_encoding === 'utf8' ? 2047 : 255));
  return `\x1b[M${String.fromCharCode(32 + button)}${cell(column)}${cell(row)}`;
}

const BASE64 = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';

/** The UTF-8 bytes of text as base64, which st's raw terminal input takes. */
export function rawInput(text: string): string {
  const bytes = new TextEncoder().encode(text);
  let out = '';
  for (let at = 0; at < bytes.length; at += 3) {
    const [a, b, c] = [bytes[at], bytes[at + 1], bytes[at + 2]];
    out += BASE64[a >> 2] + BASE64[((a & 3) << 4) | ((b ?? 0) >> 4)]
      + (b === undefined ? '=' : BASE64[((b & 15) << 2) | ((c ?? 0) >> 6)])
      + (c === undefined ? '=' : BASE64[c & 63]);
  }
  return out;
}

/**
 * Keystrokes in order, one request at a time: what is typed while a request is out joins the
 * next one, so fast typing never reorders and never waits a round trip per key.
 */
export class InputQueue {
  private waiting = '';
  private running = false;

  constructor(private readonly send: (bytes: string) => Promise<void>, private readonly failed: (error: unknown) => void) {}

  push(bytes: string): void {
    if (!bytes) return;
    this.waiting += bytes;
    if (!this.running) void this.drain();
  }

  get busy(): boolean { return this.running; }

  private async drain(): Promise<void> {
    this.running = true;
    try {
      while (this.waiting) {
        const bytes = this.waiting;
        this.waiting = '';
        // A failed request is not retried: the keys may have reached the program.
        try { await this.send(bytes); } catch (error) { this.waiting = ''; this.failed(error); }
      }
    } finally { this.running = false; }
  }
}
