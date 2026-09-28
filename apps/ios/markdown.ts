// Markdown as blocks and inline runs, following stui's `text::markdown`: real headings,
// bullets with a hanging indent, fenced code, quotes and simple tables. Line breaks in the
// source stay line breaks, because agents write them on purpose.

import { lines, trim, trimStart } from './messageText.ts';

export type Run = { text: string; bold?: boolean; code?: boolean; link?: string };
export type Block =
  | { kind: 'blank' }
  | { kind: 'text'; runs: Run[] }
  | { kind: 'heading'; level: number; runs: Run[] }
  | { kind: 'rule' }
  | { kind: 'quote'; runs: Run[] }
  | { kind: 'item'; indent: number; marker: string; runs: Run[] }
  | { kind: 'code'; language: string; lines: string[] }
  | { kind: 'table'; rows: string[][] };

export function inline(text: string): Run[] {
  const runs: Run[] = [];
  let plain = '';
  let rest = text;
  const flush = () => { if (plain) { runs.push({ text: plain }); plain = ''; } };
  while (rest) {
    const boldEnd = rest.startsWith('**') ? rest.indexOf('**', 2) : -1;
    const codeEnd = rest.startsWith('`') ? rest.indexOf('`', 1) : -1;
    const close = rest.startsWith('[') ? rest.indexOf('](', 1) : -1;
    const linkEnd = close === -1 ? -1 : rest.indexOf(')', close + 2);
    if (boldEnd !== -1) {
      flush();
      runs.push({ text: rest.slice(2, boldEnd), bold: true });
      rest = rest.slice(boldEnd + 2);
    } else if (codeEnd !== -1) {
      flush();
      runs.push({ text: rest.slice(1, codeEnd), code: true });
      rest = rest.slice(codeEnd + 1);
    } else if (linkEnd !== -1) {
      flush();
      runs.push({ text: rest.slice(1, close), link: rest.slice(close + 2, linkEnd) });
      rest = rest.slice(linkEnd + 1);
    } else {
      const [character] = Array.from(rest);
      plain += character;
      rest = rest.slice(character.length);
    }
  }
  flush();
  return runs;
}

function heading(line: string): { level: number; text: string } | null {
  let level = 0;
  while (line[level] === '#') level++;
  return level > 0 && level <= 6 && line[level] === ' ' ? { level, text: trim(line.slice(level)) } : null;
}

function listItem(line: string): { marker: string; text: string } | null {
  if (line.startsWith('- ') || line.startsWith('* ')) return { marker: '• ', text: line.slice(2) };
  const digits = /^[0-9]{1,3}\. /.exec(line);
  return digits ? { marker: digits[0], text: line.slice(digits[0].length) } : null;
}

export function markdown(input: string): Block[] {
  const source = lines(input);
  const blocks: Block[] = [];
  let index = 0;
  while (index < source.length) {
    const line = source[index];
    const trimmed = trimStart(line);
    if (trimmed.startsWith('```')) {
      const language = trim(trimmed.slice(3));
      const code: string[] = [];
      index++;
      while (index < source.length && !trimStart(source[index]).startsWith('```')) code.push(source[index++]);
      blocks.push({ kind: 'code', language, lines: code });
      index++;
      continue;
    }
    if (trimmed.startsWith('|')) {
      const rows: string[][] = [];
      while (index < source.length && trimStart(source[index]).startsWith('|')) {
        const row = trim(source[index++]);
        if (/^[|\-: ]+$/.test(row)) continue;
        rows.push(row.replace(/^\|+|\|+$/g, '').split('|').map(cell => trim(cell).replaceAll('**', '').replaceAll('`', '')));
      }
      blocks.push({ kind: 'table', rows });
      continue;
    }
    const head = heading(trimmed);
    const item = listItem(trimmed);
    if (!trimmed) blocks.push({ kind: 'blank' });
    else if (head) blocks.push({ kind: 'heading', level: head.level, runs: inline(head.text) });
    else if (trimmed === '---' || trimmed === '***' || trimmed === '___') blocks.push({ kind: 'rule' });
    else if (trimmed.startsWith('>')) blocks.push({ kind: 'quote', runs: inline(trimmed.startsWith('> ') ? trimmed.slice(2) : trimmed.slice(1)) });
    else if (item) blocks.push({ kind: 'item', indent: line.length - trimmed.length, marker: item.marker, runs: inline(item.text) });
    else blocks.push({ kind: 'text', runs: inline(trimmed) });
    index++;
  }
  while (blocks[blocks.length - 1]?.kind === 'blank') blocks.pop();
  return blocks;
}

/** Plain text of runs, for accessibility labels and copying. */
export const plainText = (runs: Run[]) => runs.map(run => run.text).join('');
