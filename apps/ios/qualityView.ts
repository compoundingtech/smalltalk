import { judgeQuality, type QualityHour, type SloTarget } from '../../clients/typescript/st3-views/slo';
const LABELS: Record<SloTarget, string> = {
  panics_max: 'Panics', silent_exits_per_100_active_hours_lt: 'Exits / 100 active h',
  first_frame_p95_ms_lt: 'First frame p95', first_real_state_p95_ms_lt: 'Real state p95',
  conversation_open_p50_ms_lt: 'Conversation p50', conversation_open_p95_ms_lt: 'Conversation p95',
  conversation_error_percent_lt: 'Open errors', key_to_redraw_p95_ms_lt: 'Typing p95',
  reconnect_p95_ms_lt: 'Reconnect p95', direct_attach_success_percent_gte: 'Direct attach',
  terminal_open_p95_ms_lt: 'Terminal p95', alert_landed_p95_ms_lt: 'Answer landed p95',
  send_echo_p99_ms_lt: 'Send shown p99', send_visible_p99_ms_lt: 'Send sent p99',
};
function numberText(value: number, key: SloTarget): string {
  if (key.includes('_ms_')) return !Number.isFinite(value) ? '>32s' : value >= 1000 ? `${value / 1000}s` : `${value}ms`;
  if (key.includes('percent')) return `${Math.round(value * 10) / 10}%`;
  return `${Math.round(value * 10) / 10}`;
}
export function qualityRows(last: QualityHour | null, day: QualityHour | null) {
  const hour = last ? judgeQuality(last) : [], daily = day ? judgeQuality(day) : [];
  const reference = last ?? day;
  if (!reference) return [];
  return judgeQuality(reference).map((row, i) => ({
    key: row.target, label: LABELS[row.target],
    target: `${row.target.endsWith('_gte') ? '≥' : row.target.endsWith('_max') ? '≤' : '<'}${numberText(row.limit, row.target)}`,
    hour: hour[i]?.value == null ? '—' : numberText(hour[i].value!, row.target),
    day: daily[i]?.value == null ? '—' : numberText(daily[i].value!, row.target),
    hourPass: hour[i]?.pass ?? null, dayPass: daily[i]?.pass ?? null,
  }));
}

/** Only windows represented on this screen or its visible native tab badge. No temporary arrays. */
export function qualityVisibility(name: string, paired: boolean, target: string | undefined, agents: readonly { id: string }[], sessions: readonly { id: string; owner_id?: string | null; state?: string; managed?: boolean }[]): number {
  if (!paired || name === 'Terminal') return 0;
  if (name === 'Agents') return 3;
  if (name !== 'Conversation') return 1;
  let owner: string | null | undefined;
  for (const session of sessions) if (session.id === target) { if (session.state === 'running' && session.managed !== false) owner = session.owner_id; break; }
  for (const agent of agents) if (agent.id === target || agent.id === owner) return 3;
  return 0;
}
