import type { Attention } from '../../clients/typescript/st3-client';
import { ago } from './presentation';
import { theme } from './theme';

// Home, drawn the way stui draws it (crates/stui/src/ui/screens.rs home_list, adapt.rs
// attention): the requests and reviews st holds for the person that are not resolved, grouped by
// how urgently someone is waiting. Messages stay in conversations, and st sends a fault to the
// agent that owns it, so neither is ever on Home.

export const TIERS = ['stopped', 'today'] as const;
export type Tier = typeof TIERS[number];
export function tierTitle(tier: Tier): string {
  switch (tier) {
    case 'stopped': return 'somebody is stopped on you';
    case 'today': return 'today';
  }
}

export type HomeKind = 'review' | 'launch' | 'revision' | 'request';
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
/** Where Home shows an item, or null for a kind that is not a request or a review. */
export function homeKind(item: Kindish): { tier: Tier; kind: HomeKind } | null {
  switch (item.attention_kind as string) {
    case 'human-gate': return { tier: 'stopped', kind: 'review' };
    case 'person-step':
    case 'agent-request': return { tier: 'stopped', kind: 'request' };
    case 'launch-approval': return { tier: 'today', kind: 'launch' };
    case 'revision-approval': return { tier: 'today', kind: 'revision' };
    default: return null;
  }
}

export function kindGlyph(_kind: HomeKind): { glyph: string; color: string } {
  return { glyph: '◆', color: theme.person };
}

export const HOME_LEGEND: ReadonlyArray<{ glyph: string; color: string; word: string }> = [
  { glyph: '◆', color: theme.person, word: 'decide' },
];

/** Whether Home shows an item: it is not resolved and, when st names a person, it is for this one. */
/** The person a session acts for: a paired device acts as `person/NAME/session/ID`. */
export function sessionPerson(actor: string | undefined): string | undefined {
  return actor?.replace(/\/session\/.*$/, '');
}

export function onHome(item: Pick<Attention, 'state' | 'person_id'>, actor: string | undefined): boolean {
  const person = sessionPerson(actor);
  return item.state !== 'resolved' && (!person || !item.person_id || item.person_id === person);
}

export function homeRows(items: Attention[], actor: string | undefined, now = Date.now()): HomeRow[] {
  const rows = items.filter(item => onHome(item, actor)).flatMap((item): HomeRow[] => {
    const place = homeKind(item);
    if (!place) return [];
    const step = item.step_run_id?.split('/').pop();
    return [{ item, ...place, ...kindGlyph(place.kind), title: cleanTitle(item.title), waiting: step ? `step ${step}` : null, age: ago(item.requested_at, now) }];
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
