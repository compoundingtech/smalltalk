// What the Usage tab shows, as stui's Usage tab shows it (crates/stui/src/ui/usage.rs): each
// account's spend and limits, a summary, then every group, the largest first. Row ids are the
// subject a group names (agent/…, mission/…, step-run/…, machine/…) or account/…, model/… and
// usage/no-… for what st has no subject for, so a detail screen can be opened by id alone.
import type { UsageLimit, UsageRow } from '../../clients/typescript/st3-client';

export const BY = ['agent', 'mission', 'step', 'model', 'account', 'host'] as const;
export type By = typeof BY[number];
export const nextBy = (by: By): By => BY[(BY.indexOf(by) + 1) % BY.length];

/** The periods to choose from, in hours: a day, a week and thirty days. */
export const PERIODS = [24, 168, 720] as const;
export const periodName = (hours: number) => hours === 24 ? 'the last 24 hours' : hours % 24 === 0 ? `the last ${hours / 24} days` : `the last ${hours} hours`;

export type Total = { tokens: number; input: number; output: number; cacheWrite: number; cached: number; costMicrousd: number; unpriced: number };
const empty = (): Total => ({ tokens: 0, input: 0, output: 0, cacheWrite: 0, cached: 0, costMicrousd: 0, unpriced: 0 });
function add(total: Total, row: UsageRow): Total {
  total.tokens += row.total_tokens; total.input += row.input_tokens; total.output += row.output_tokens;
  total.cacheWrite += row.cache_write_tokens; total.cached += row.cached_tokens;
  total.costMicrousd += row.cost_microusd; total.unpriced += row.unpriced_tokens;
  return total;
}
export const totalOf = (rows: readonly UsageRow[]): Total => rows.reduce(add, empty());

export function money(microusd: number): string {
  const dollars = microusd / 1_000_000;
  if (microusd === 0) return '$0';
  if (dollars < 0.01) return '<$0.01';
  return dollars < 100 ? `$${dollars.toFixed(2)}` : `$${Math.round(dollars)}`;
}
/** A cost, with a + when some tokens had no price: the true cost is higher, never lower. */
export const cost = (total: Total) => `${money(total.costMicrousd)}${total.unpriced > 0 ? '+' : ''}`;

export function tokens(count: number): string {
  for (const [size, unit] of [[1e9, 'B'], [1e6, 'M'], [1e3, 'k']] as const) {
    if (count >= size) { const value = count / size; return value < 10 ? `${value.toFixed(1)}${unit}` : `${Math.round(value)}${unit}`; }
  }
  return `${count}`;
}

/** `mission-run/NAME/RUN` names the mission `mission/NAME`. */
export function missionOf(run: string): string {
  const name = run.replace(/^mission-run\//, '');
  const cut = name.lastIndexOf('/');
  return `mission/${cut > 0 ? name.slice(0, cut) : name}`;
}

/** The id a row is grouped under. */
export function key(row: UsageRow, by: By): string {
  const none = `usage/no-${by}`;
  const known = (value: string | null | undefined) => value ? value : undefined;
  switch (by) {
    case 'agent': return row.agent;
    case 'mission': { const run = known(row.mission_run); return run ? missionOf(run) : none; }
    case 'step': return known(row.step) ?? none;
    case 'model': { const model = known(row.model); return model ? `model/${model}` : none; }
    case 'account': { const account = known(row.account); return account ? `account/${account}` : none; }
    case 'host': { const host = known(row.host); return host ? `machine/${host}` : none; }
  }
}

/** The grouping an id belongs to, from its prefix. */
export function byOf(id: string): By | null {
  if (id.startsWith('usage/no-')) { const by = id.slice('usage/no-'.length); return (BY as readonly string[]).includes(by) ? by as By : null; }
  const prefix = id.split('/')[0];
  return ({ agent: 'agent', mission: 'mission', 'step-run': 'step', model: 'model', account: 'account', machine: 'host' } as Record<string, By>)[prefix] ?? null;
}

/** Each group's spend, the largest first. */
export function groups(rows: readonly UsageRow[], by: By): Array<{ id: string; total: Total }> {
  const totals = new Map<string, Total>();
  for (const row of rows) { const id = key(row, by); totals.set(id, add(totals.get(id) ?? empty(), row)); }
  return [...totals].map(([id, total]) => ({ id, total }))
    .sort((a, b) => b.total.costMicrousd - a.total.costMicrousd || b.total.tokens - a.total.tokens || a.id.localeCompare(b.id));
}

export type Names = { agents: ReadonlyMap<string, string>; missions: ReadonlyMap<string, string> };

/** A group as a person reads it: an agent's or mission's name, a step with its mission, an account by provider. */
export function label(id: string, names: Names, rows: readonly UsageRow[]): string {
  if (id.startsWith('account/')) {
    const account = id.slice('account/'.length);
    const cut = account.indexOf('/');
    if (cut < 0) return account;
    const driver = account.slice(0, cut), digest = account.slice(cut + 1);
    return digest === 'unknown' ? `${driver} · account not named` : `${driver} · ${digest.slice(0, 8)}`;
  }
  if (id.startsWith('usage/no-')) {
    const what = id.slice('usage/no-'.length);
    return what === 'account' ? 'no account recorded (before st kept accounts)'
      : what === 'mission' ? 'no mission (standing seats)'
      : what === 'step' ? 'no step (standing seats)' : `unknown ${what}`;
  }
  if (id.startsWith('agent/')) return names.agents.get(id) ?? id.slice('agent/'.length);
  if (id.startsWith('mission/')) return names.missions.get(id) ?? id.slice('mission/'.length);
  if (id.startsWith('step-run/')) {
    const step = id.split('/').pop() ?? id;
    const run = rows.find(row => row.step === id)?.mission_run;
    return run ? `${label(missionOf(run), names, rows)} · ${step}` : step;
  }
  return id.includes('/') ? id.slice(id.indexOf('/') + 1) : id;
}

/** Every account that spent in the period or reported limits, the largest spender first, unattributed spend last. */
export function accounts(rows: readonly UsageRow[], limits: readonly UsageLimit[]): Array<{ id: string; total: Total; limit?: UsageLimit }> {
  const found = groups(rows, 'account').map(({ id, total }) => ({ id, total, limit: limits.find(limit => id === `account/${limit.account}`) }));
  for (const limit of limits) if (!found.some(entry => entry.id === `account/${limit.account}`)) found.push({ id: `account/${limit.account}`, total: empty(), limit });
  return [...found.filter(entry => !entry.id.startsWith('usage/')), ...found.filter(entry => entry.id.startsWith('usage/'))];
}

export type Tone = 'ok' | 'warning' | 'fault' | 'quiet';
/** A share of a limit, coloured as stui colours it: yellow from 75%, red from 90%. */
export function share(percent: number | null | undefined): { text: string; tone: Tone } {
  if (percent === null || percent === undefined) return { text: '?', tone: 'quiet' };
  return { text: `${Math.round(percent)}%`, tone: percent >= 90 ? 'fault' : percent >= 75 ? 'warning' : 'ok' };
}

/** A length of time as a person says it: 40s, 12m, 5h, 3d. */
export function span(ms: number): string {
  const seconds = Math.max(0, Math.floor(ms / 1000));
  if (seconds < 60) return `${seconds}s`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m`;
  if (seconds < 86_400) return `${Math.floor(seconds / 3600)}h`;
  return `${Math.floor(seconds / 86_400)}d`;
}

/** An account's limits as stui words them: weekly first, since that is the one that stops seats. */
export function limitLine(limit: UsageLimit, now: number) {
  return {
    weekly: share(limit.weekly_percent),
    fiveHour: share(limit.five_hour_percent),
    resets: limit.weekly_resets_at_unix_ms ? `resets in ${span(limit.weekly_resets_at_unix_ms - now)}` : '',
    measured: `measured ${span(now - limit.measured_at_unix_ms)} ago`,
  };
}
