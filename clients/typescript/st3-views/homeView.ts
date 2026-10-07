import type { Attention } from '@smalltalk/st3-client';
import { ago } from './time.ts';

// Home, drawn the way stui draws it (crates/stui/src/ui/screens.rs home_list, adapt.rs
// attention): the requests and reviews st holds for the person that are not resolved, grouped by
// how urgently someone is waiting. Messages stay in conversations, and st sends a fault to the
// agent that owns it, so neither is ever on Home.

export const TIERS = ['stopped', 'today', 'later'] as const;
export type Tier = typeof TIERS[number];
export function tierTitle(tier: Tier): string {
  switch (tier) {
    case 'stopped': return 'somebody is stopped on you';
    case 'today': return 'today';
    case 'later': return 'when there is time';
  }
}

export type HomeKind = 'review' | 'launch' | 'revision' | 'request' | 'update';
/** Semantic colors; renderers resolve them through their own palette. */
export type HomeColor = 'person' | 'green';
export type HomeRow = {
  item: KeptAttention;
  tier: Tier;
  kind: HomeKind;
  glyph: string;
  color: HomeColor;
  title: string;
  /** Who or what is waiting, when st says: `step build`. */
  waiting: string | null;
  age: string;
};

type Kindish = Pick<Attention, 'attention_kind' | 'priority'> & { update?: Attention['update'] };
/** Where Home shows an item, or null for a kind that is not a request or a review. */
export function homeKind(item: Kindish): { tier: Tier; kind: HomeKind } | null {
  switch (item.attention_kind as string) {
    case 'human-gate': return { tier: 'stopped', kind: 'review' };
    // Information the person asked for (`st work update`): nothing waits on it.
    case 'person-step': return item.update ? { tier: 'later', kind: 'update' } : { tier: 'stopped', kind: 'request' };
    case 'agent-request': return { tier: 'stopped', kind: 'request' };
    // A prompt a harness is waiting on: answered with its own action (`prompt.respond`).
    case 'harness-prompt': return { tier: 'stopped', kind: 'request' };
    case 'launch-approval': return { tier: 'today', kind: 'launch' };
    case 'revision-approval': return { tier: 'today', kind: 'revision' };
    default: return null;
  }
}

export function kindGlyph(kind: HomeKind): { glyph: string; color: HomeColor } {
  return kind === 'update' ? { glyph: '✦', color: 'green' } : { glyph: '◆', color: 'person' };
}

export const HOME_LEGEND: ReadonlyArray<{ glyph: string; color: HomeColor; word: string }> = [
  { glyph: '◆', color: 'person', word: 'decide' },
  { glyph: '✦', color: 'green', word: 'to read' },
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

/** An item st stopped listing (or resolved) that stays on Home until the person clears it. */
export type KeptAttention = Attention & { closedElsewhere?: boolean };

/**
 * What stays on Home when st's list changes: nothing leaves by itself (Nathan, 2026-10-07).
 * An item shown before that st no longer lists as open, and that the person did not act on from
 * here, is kept, marked closed, with no actions, until the person clears it.
 */
export const RECENTLY_CLOSED = 5;

export function keepClosed(previous: readonly KeptAttention[], next: readonly Attention[], acted: ReadonlySet<string>, actor?: string): KeptAttention[] {
  const kept: KeptAttention[] = [...next];
  const open = new Set(next.filter(item => onHome(item, actor)).map(item => item.id));
  // Items kept before go first, so the newly closed sit after them and the oldest drops first.
  for (const old of [...previous.filter(item => item.closedElsewhere), ...previous.filter(item => !item.closedElsewhere)]) {
    if (open.has(old.id) || acted.has(old.id) || !onHome(old, actor)) continue;
    const marked: KeptAttention = old.closedElsewhere ? old : { ...old, closedElsewhere: true, actions: [] };
    const at = kept.findIndex(item => item.id === old.id);
    if (at >= 0) kept[at] = marked; else kept.push(marked);
  }
  // Home lists only the latest few closed items under "Recently closed".
  let over = kept.filter(item => item.closedElsewhere).length - RECENTLY_CLOSED;
  return over > 0 ? kept.filter(item => !(item.closedElsewhere && over-- > 0)) : kept;
}

export function homeRows(items: KeptAttention[], actor: string | undefined, now = Date.now()): HomeRow[] {
  const rows = items.filter(item => onHome(item, actor)).flatMap((item): HomeRow[] => {
    const place = homeKind(item);
    if (!place) return [];
    const step = item.step_run_id?.split('/').pop();
    return [{ item, ...place, ...kindGlyph(place.kind), title: cleanTitle(item.title), waiting: item.closedElsewhere ? 'closed; stays until you clear it' : step ? `step ${step}` : null, age: ago(item.requested_at, now) }];
  });
  // A stable sort by tier keeps st's order within each tier; what st closed sits last, apart.
  const rank = (row: HomeRow) => (row.item.closedElsewhere ? TIERS.length : 0) + TIERS.indexOf(row.tier);
  return rows.map((row, index) => ({ row, index })).sort((a, b) => rank(a.row) - rank(b.row) || a.index - b.index).map(({ row }) => row);
}

export type HomeSection = { tier: Tier; title: string; count: number; rows: HomeRow[]; closed?: boolean };
export function homeSections(rows: HomeRow[]): HomeSection[] {
  const sections: HomeSection[] = [];
  for (const row of rows) {
    const closed = !!row.item.closedElsewhere;
    const last = sections.at(-1);
    // Items st closed while they were shown stay under a heading of their own, never mixed with
    // what needs the person (Nathan, 2026-10-07: "2 need you", 4 listed).
    if (closed) {
      if (last?.closed) { last.rows.push(row); last.count++; }
      else sections.push({ tier: row.tier, title: 'Recently closed: clear each', count: 1, rows: [row], closed: true });
    } else if (last?.tier === row.tier && !last.closed) { last.rows.push(row); last.count++; }
    else sections.push({ tier: row.tier, title: tierTitle(row.tier), count: 1, rows: [row] });
  }
  return sections;
}

function cleanTitle(title: string): string {
  return title.replace(/^\[PING\]\s*\?\s*/, '').replace(/\s*\[id:message\/[^\]]+\]\s*$/, '').trim() || title;
}
