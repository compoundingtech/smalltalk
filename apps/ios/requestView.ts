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
