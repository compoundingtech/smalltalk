// Block markdown the way stui renders an assistant reply (crates/stui/src/ui/text.rs markdown
// and inline): headings, bullets with a hanging marker, fenced code, quotes, rules, and tables
// kept as monospace rows. Anything else is a plain line.

export type Run = { text: string; style: 'plain' | 'bold' | 'code' | 'link'; url?: string; path?: string };
export type Block =
  | { kind: 'blank' }
  | { kind: 'text'; runs: Run[] }
  | { kind: 'heading'; level: number; runs: Run[] }
  | { kind: 'item'; indent: number; marker: string; runs: Run[] }
  | { kind: 'quote'; runs: Run[] }
  | { kind: 'rule' }
  | { kind: 'fence'; text: string }
  | { kind: 'code'; text: string }
  | { kind: 'table'; text: string };

// A written-out web address: it runs to whitespace, then gives back the punctuation that ended the
// sentence around it (and a closing bracket nothing opened).
function address(rest: string): string | null {
  const found = /^https?:\/\/[^\s<>]+/.exec(rest);
  if (!found) return null;
  let url = found[0];
  for (;;) {
    const last = url[url.length - 1];
    const open = { ')': '(', ']': '[' }[last];
    const unmatched = open !== undefined && (url.split(last).length - 1) > (url.split(open).length - 1);
    if (/[.,;:!?'"*>]/.test(last) || unmatched) url = url.slice(0, -1);
    else break;
  }
  return url.length > 'https://'.length - 1 ? url : null;
}

/** The file a markdown link's target names, when it is an absolute or home path (or a file: URL). */
export function filePath(target: string): string | undefined {
  const value = target.trim().replace(/^file:\/\//, '');
  return /^(\/|~\/)[^\s]/.test(value) ? value : undefined;
}

export function inline(text: string): Run[] {
  const runs: Run[] = [];
  let plain = '';
  const flush = () => { if (plain) { runs.push({ text: plain, style: 'plain' }); plain = ''; } };
  let rest = text;
  while (rest) {
    let match: RegExpExecArray | null;
    if (rest.startsWith('**') && rest.indexOf('**', 2) > 1) {
      const end = rest.indexOf('**', 2);
      flush(); runs.push({ text: rest.slice(2, end), style: 'bold' }); rest = rest.slice(end + 2);
    } else if (rest.startsWith('`') && rest.indexOf('`', 1) > 0) {
      const end = rest.indexOf('`', 1);
      flush(); runs.push({ text: rest.slice(1, end), style: 'code' }); rest = rest.slice(end + 1);
    } else if ((match = /^\[([^\]]*)\]\(([^)]*)\)/.exec(rest))) {
      // An address opens; a path names a file on the machine the writer works on (an agent's
      // `[spec](/home/…/spec.md)`), which this phone cannot read but can copy.
      flush(); runs.push({ text: match[1], style: 'link', url: /^https?:\/\//.test(match[2]) ? match[2] : undefined, ...(filePath(match[2]) ? { path: filePath(match[2]) } : {}) }); rest = rest.slice(match[0].length);
    } else if ((match = /^https?:\/\//.exec(rest)) && (plain === '' || /[\s(\["'*]$/.test(plain)) && address(rest)) {
      const url = address(rest)!;
      flush(); runs.push({ text: url, style: 'link', url }); rest = rest.slice(url.length);
    } else {
      const character = String.fromCodePoint(rest.codePointAt(0)!);
      plain += character; rest = rest.slice(character.length);
    }
  }
  flush();
  return runs;
}

function listItem(line: string): { marker: string; item: string } | null {
  if (line.startsWith('- ') || line.startsWith('* ')) return { marker: '• ', item: line.slice(2) };
  const digits = /^(\d{1,3})\. /.exec(line);
  return digits ? { marker: `${digits[1]}. `, item: line.slice(digits[0].length) } : null;
}

export function markdown(input: string): Block[] {
  const source = input.replace(/\r\n/g, '\n').split('\n');
  const blocks: Block[] = [];
  for (let index = 0; index < source.length; index++) {
    const line = source[index];
    const trimmed = line.trimStart();
    if (trimmed.startsWith('```')) {
      blocks.push({ kind: 'fence', text: `\`\`\`${trimmed.slice(3).trim()}` });
      for (index++; index < source.length && !source[index].trimStart().startsWith('```'); index++) blocks.push({ kind: 'code', text: source[index] });
      blocks.push({ kind: 'fence', text: '```' });
      continue;
    }
    if (trimmed.startsWith('|')) {
      for (; index < source.length && source[index].trimStart().startsWith('|'); index++) {
        const row = source[index].trim();
        if (![...row].every(character => '|-: '.includes(character))) blocks.push({ kind: 'table', text: row });
      }
      index--;
      continue;
    }
    const heading = /^(#{1,6}) (.*)$/.exec(trimmed);
    const item = listItem(trimmed);
    if (!trimmed) blocks.push({ kind: 'blank' });
    else if (heading) blocks.push({ kind: 'heading', level: heading[1].length, runs: inline(heading[2].trim()) });
    else if (['---', '***', '___'].includes(trimmed)) blocks.push({ kind: 'rule' });
    else if (trimmed.startsWith('>')) blocks.push({ kind: 'quote', runs: inline(trimmed.replace(/^> ?/, '')) });
    else if (item) blocks.push({ kind: 'item', indent: line.length - trimmed.length, marker: item.marker, runs: inline(item.item) });
    else blocks.push({ kind: 'text', runs: inline(trimmed) });
  }
  while (blocks.at(-1)?.kind === 'blank') blocks.pop();
  while (blocks[0]?.kind === 'blank') blocks.shift();
  return blocks;
}
