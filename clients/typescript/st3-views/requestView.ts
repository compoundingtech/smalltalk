// A request card as stui shows one (crates/stui/src/ui/screens.rs, `report` and the Request
// card): a JSON report reads as its telling fields, a yes-or-no question gets Yes and No, and
// every answer (Nothing to do included) completes the waiting step with a short reason.

/** Attention kinds that are an agent waiting on the person for an answer. */
export const isRequest = (kind: string) => kind === 'person-step' || kind === 'agent-request';

/** A question that ends in `?` can be answered in one tap. */
export const yesNo = (title: string) => title.trimEnd().endsWith('?');

/** What each answer sends as the step's reason, as stui sends it. */
export const ANSWERS = { yes: 'Yes', no: 'No', nothing: 'Nothing for me to do here.' } as const;

export type ReportRow = { key: string; value: string; tone: 'fault' | 'text' | 'soft' };
export type Report = { before: string; rows: ReportRow[]; more: number };

// Fields that say what happened, shown first; then up to four others, then a count.
const TELLING = ['status', 'state', 'outcome', 'result', 'error', 'reason', 'message', 'summary', 'commit', 'sha', 'version', 'host', 'run', 'url'];
const OTHERS = 4;

/** A report pasted in as JSON, as its telling fields instead of a wall of braces; null otherwise. */
export function report(text: string): Report | null {
  const start = text.indexOf('{'), end = text.lastIndexOf('}');
  if (start < 0 || end <= start) return null;
  let fields: unknown;
  try { fields = JSON.parse(text.slice(start, end + 1)); } catch { return null; }
  if (!fields || typeof fields !== 'object' || Array.isArray(fields)) return null;
  const record = fields as Record<string, unknown>;
  const shown = (value: unknown) => typeof value === 'string' ? value.split('\n')[0] : JSON.stringify(value);
  let telling = TELLING.filter(key => key in record);
  if (!telling.length) telling = Object.keys(record).slice(0, OTHERS);
  const rows: ReportRow[] = telling.map(key => ({
    key, value: shown(record[key]),
    tone: key === 'error' || record[key] === 'failed' || record[key] === 'error' ? 'fault' : 'text',
  }));
  const rest = Object.keys(record).filter(key => !telling.includes(key));
  for (const key of rest.slice(0, OTHERS)) rows.push({ key, value: shown(record[key]), tone: 'soft' });
  return { before: text.slice(0, start).trim(), rows, more: Math.max(0, rest.length - OTHERS) };
}

/** A question as agents write one, "Recommend: …⏎Why: …⏎Answer …", given room to read, as stui's
 * `spaced`: a blank line between consecutive lines of prose, and a short leading label ("Why:")
 * in bold. Lists, quotes, tables, headings and code keep their lines together. */
export function spaced(question: string): string {
  const block = (line: string) => {
    const trimmed = line.trimStart();
    return /^[-*>|#]/.test(trimmed) || trimmed.startsWith('```') || /^\d{1,3}\. /.test(trimmed);
  };
  const label = (line: string) => {
    const at = line.indexOf(': ');
    if (at <= 0) return line;
    const head = line.slice(0, at);
    const ok = head.length <= 24 && head.split(/\s+/).filter(Boolean).length <= 3 && /^[A-Z]/.test(head) && !/[*`[]/.test(head);
    return ok ? `**${head}:** ${line.slice(at + 2)}` : line;
  };
  const out: string[] = [];
  let fenced = false, previousProse = false;
  for (const line of question.replace(/\r\n/g, '\n').split('\n')) {
    if (line.trimStart().startsWith('```')) fenced = !fenced;
    const prose = !fenced && line.trim() !== '' && !block(line);
    if (prose && previousProse) out.push('');
    out.push(prose ? label(line) : line);
    previousProse = prose;
  }
  return out.join('\n');
}

type Request = { type: string; custom?: boolean; answers?: { id: string; outcome?: string | null }[] };

/**
 * The typed answer for a person step, as stui sends it: the named answer chosen (with the words
 * when it requests changes), or the person's own words where the request takes them. Words
 * alone on a structured request are refused by st (`answer-required`), which left an answered
 * item on Home (Nathan, 2026-10-03). A string says why words cannot answer it.
 */
export function personAnswer(request: Request | null | undefined, chosen: string | undefined, words: string): { id?: string; text?: string } | undefined | string {
  const text = words.trim();
  if (chosen) {
    const changes = request?.answers?.some(answer => answer.id === chosen && answer.outcome === 'request_changes');
    return changes ? { id: chosen, text } : { id: chosen };
  }
  if (!request) return undefined;
  if (request.type === 'feedback' || (request.type === 'choice' && request.custom)) return { text };
  const changes = request.answers?.find(answer => answer.outcome === 'request_changes');
  return changes ? { id: changes.id, text } : 'This asks you to pick one of its answers.';
}
