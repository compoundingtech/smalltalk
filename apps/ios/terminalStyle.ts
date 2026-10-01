import type { TerminalColor, TerminalRun } from '../../clients/typescript/st3-client/Models.generated';

// The first 16 entries are the theme's ANSI colors; 16-231 are the xterm color cube and
// 232-255 its gray ramp.
const ANSI = ['#1b2b36', '#e06c75', '#98c379', '#e5c07b', '#61afef', '#c678dd', '#56b6c2', '#d6dee3', '#5c6f7b', '#ff7b86', '#b5e890', '#ffd68a', '#82c4ff', '#e09cf0', '#7fd6e0', '#f3f7fa'];
const hex = (value: number) => value.toString(16).padStart(2, '0');

export function terminalColor(color: TerminalColor): string {
  if (typeof color === 'string') return color;
  if (color < 16) return ANSI[color];
  if (color < 232) {
    const level = (step: number) => (step === 0 ? 0 : 55 + step * 40);
    const index = color - 16;
    return `#${hex(level(Math.floor(index / 36)))}${hex(level(Math.floor(index / 6) % 6))}${hex(level(index % 6))}`;
  }
  const gray = 8 + (color - 232) * 10;
  return `#${hex(gray)}${hex(gray)}${hex(gray)}`;
}

export type TerminalTextStyle = {
  color: string;
  backgroundColor?: string;
  fontWeight?: 'bold';
  fontStyle?: 'italic';
  textDecorationLine?: 'underline';
  opacity?: number;
};

export function terminalRunStyle(run: TerminalRun, defaults: { fg: string; bg: string }): TerminalTextStyle {
  let color = run.fg === undefined ? defaults.fg : terminalColor(run.fg);
  let background = run.bg === undefined ? undefined : terminalColor(run.bg);
  if (run.inverse) [color, background] = [background ?? defaults.bg, color];
  return {
    color,
    ...(background ? { backgroundColor: background } : {}),
    ...(run.bold ? { fontWeight: 'bold' as const } : {}),
    ...(run.italic ? { fontStyle: 'italic' as const } : {}),
    ...(run.underline ? { textDecorationLine: 'underline' as const } : {}),
    ...(run.dim ? { opacity: 0.6 } : {}),
  };
}

/** A line's runs with the cell at `column` drawn as the cursor (inverse), padding a short line. */
export function withCursor(runs: TerminalRun[], column: number): TerminalRun[] {
  const out: TerminalRun[] = [];
  let at = 0, placed = false;
  for (const run of runs) {
    const chars = Array.from(run.text);
    if (placed || column < at || column >= at + chars.length) { out.push(run); at += chars.length; continue; }
    const offset = column - at;
    if (offset) out.push({ ...run, text: chars.slice(0, offset).join('') });
    out.push({ ...run, text: chars[offset], inverse: run.inverse ? undefined : true });
    if (offset + 1 < chars.length) out.push({ ...run, text: chars.slice(offset + 1).join('') });
    at += chars.length;
    placed = true;
  }
  if (!placed) {
    if (column > at) out.push({ text: ' '.repeat(column - at) });
    out.push({ text: ' ', inverse: true });
  }
  return out;
}
