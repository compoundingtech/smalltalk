import type { Attention } from '../../clients/typescript/st3-client';
import { ago } from './presentation';
import { theme } from './theme';

// Home, drawn the way stui draws it (crates/stui/src/ui/screens.rs home_list, adapt.rs
// attention): every attention item st holds for the person that is not resolved, including
// unread messages, grouped by how urgently someone is waiting.

export const TIERS = ['stopped', 'alert', 'today', 'later'] as const;
export type Tier = typeof TIERS[number];
export function tierTitle(tier: Tier): string {
  switch (tier) {
    case 'stopped': return 'somebody is stopped on you';
    case 'alert': return 'something broke';
    case 'today': return 'today';
    case 'later': return 'when there is time';
  }
}

export type HomeKind = 'review' | 'launch' | 'revision' | 'message' | 'request' | 'fault';
export type HomeRow = {
  item: Attention;
  tier: Tier;
  kind: HomeKind;
  glyph: string;
  color: string;
  title: string;
  /** Who or what is waiting, when st says: `step build`. */
  waiting: string | null;
  age: string;
};

type Kindish = Pick<Attention, 'attention_kind' | 'priority'>;
export function homeKind(item: Kindish): { tier: Tier; kind: HomeKind } {
  switch (item.attention_kind as string) {
    case 'human-gate': return { tier: 'stopped', kind: 'review' };
    case 'agent-request': return { tier: 'stopped', kind: 'request' };
    case 'launch-approval': return { tier: 'today', kind: 'launch' };
    case 'revision-approval': return { tier: 'today', kind: 'revision' };
    case 'unread-message': return { tier: 'later', kind: 'message' };
    // Anything st adds later reads as a fault raised for the person, as stui reads it.
    default: return { tier: item.priority === 'critical' || item.priority === 'high' ? 'alert' : 'today', kind: 'fault' };
  }
}

export function kindGlyph(kind: HomeKind): { glyph: string; color: string } {
  if (kind === 'fault') return { glyph: '✕', color: theme.fault };
  if (kind === 'message') return { glyph: '✉', color: theme.sapphire };
  return { glyph: '◆', color: theme.person };
}

export const HOME_LEGEND: ReadonlyArray<{ glyph: string; color: string; word: string }> = [
  { glyph: '◆', color: theme.person, word: 'decide' },
  { glyph: '✕', color: theme.fault, word: 'fault' },
  { glyph: '✉', color: theme.sapphire, word: 'message' },
];

/** Whether Home shows an item: it is not resolved and, when st names a person, it is for this one. */
export function onHome(item: Pick<Attention, 'state' | 'person_id'>, actor: string | undefined): boolean {
  return item.state !== 'resolved' && (!actor || !item.person_id || item.person_id === actor);
}

export function homeRows(items: Attention[], actor: string | undefined, now = Date.now()): HomeRow[] {
  const rows = items.filter(item => onHome(item, actor)).map((item): HomeRow => {
    const { tier, kind } = homeKind(item);
    const step = item.step_run_id?.split('/').pop();
    return { item, tier, kind, ...kindGlyph(kind), title: cleanTitle(item.title), waiting: step ? `step ${step}` : null, age: ago(item.requested_at, now) };
  });
  // A stable sort by tier keeps st's order within each tier.
  return rows.map((row, index) => ({ row, index })).sort((a, b) => TIERS.indexOf(a.row.tier) - TIERS.indexOf(b.row.tier) || a.index - b.index).map(({ row }) => row);
}

export type HomeSection = { tier: Tier; title: string; count: number; rows: HomeRow[] };
export function homeSections(rows: HomeRow[]): HomeSection[] {
  const sections: HomeSection[] = [];
  for (const row of rows) {
    const last = sections.at(-1);
    if (last?.tier === row.tier) { last.rows.push(row); last.count++; }
    else sections.push({ tier: row.tier, title: tierTitle(row.tier), count: 1, rows: [row] });
  }
  return sections;
}

function cleanTitle(title: string): string {
  return title.replace(/^\[PING\]\s*\?\s*/, '').replace(/\s*\[id:message\/[^\]]+\]\s*$/, '').trim() || title;
}
