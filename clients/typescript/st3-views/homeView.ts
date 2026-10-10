import type { Attention, TimelineEntry } from '@smalltalk/st3-client';
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

export type HomeKind = 'review' | 'launch' | 'revision' | 'request' | 'update' | 'prompt' | 'login' | 'condition';
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
    case 'condition': return { tier: 'today', kind: 'condition' };
    case 'agent-request': return { tier: 'stopped', kind: 'request' };
    case 'launch-approval': return { tier: 'today', kind: 'launch' };
    case 'revision-approval': return { tier: 'today', kind: 'revision' };
    // A native prompt a seat's harness waits on, and a provider sign-in a seat needs: alerts in
    // the seat's conversation, and on Home beside every other thing that waits on the person.
    case 'harness-prompt': return { tier: 'stopped', kind: 'prompt' };
    case 'harness-login': return { tier: 'stopped', kind: 'login' };
    default: return null;
  }
}

export function kindGlyph(kind: HomeKind): { glyph: string; color: HomeColor } {
  return kind === 'condition' ? { glyph: '!', color: 'person' } : kind === 'update' ? { glyph: '✦', color: 'green' } : { glyph: '◆', color: 'person' };
}

export const HOME_LEGEND: ReadonlyArray<{ glyph: string; color: HomeColor; word: string }> = [
  { glyph: '◆', color: 'person', word: 'decide' },
  { glyph: '✦', color: 'green', word: 'to read' },
  { glyph: '!', color: 'person', word: 'breached' },
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
    return [{ item, ...place, ...kindGlyph(place.kind), title: cleanTitle(item.title).trim() || (item.attention_kind === 'condition' ? 'Condition breached' : item.title), waiting: item.closedElsewhere ? 'closed; stays until you clear it' : step ? `step ${step}` : null, age: ago(item.requested_at, now) }];
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

// Alerts: whatever blocks or waits on the person (an ask, a human gate, a launch or revision
// approval, a native harness prompt, a provider sign-in). An update asks nothing, and a message
// stays in its conversation, so neither is an alert. The person reads them in two places: the
// count (nothing at zero, `N alerts` otherwise) and the conversation of the agent each belongs to.

type Alertish = Pick<Attention, 'attention_kind'> & { alert?: boolean; update?: unknown };

/** Whether the item is an alert. A daemon that predates alerts does not say, and then everything but an update or an unread message is one. */
export function isAlert(item: Alertish): boolean {
  return item.alert ?? (!item.update && item.attention_kind !== 'unread-message');
}

/**
 * The alerts the person can see: open, theirs, an alert, of a kind the app shows, and not an item
 * st closed that only waits to be cleared. The same rows Home lists, so the count and the list agree.
 */
export function openAlerts(items: readonly KeptAttention[], actor: string | undefined): KeptAttention[] {
  return items.filter(item => !item.closedElsewhere && onHome(item, actor) && isAlert(item) && homeKind(item) !== null);
}

/** `3 alerts`, `1 alert`, and nothing at all for none. */
export function alertsHeading(count: number): string {
  return count <= 0 ? '' : count === 1 ? '1 alert' : `${count} alerts`;
}

/** The agent conversations an alert shows in, the one it belongs to first. */
export function alertConversations(item: Pick<Attention, 'source_id'> & { conversation_id?: string; conversation_ids?: readonly string[]; requester_id?: string | null }): string[] {
  const named = [...(item.conversation_id ? [item.conversation_id] : []), ...(item.conversation_ids ?? [])];
  // A daemon that predates alerts names no conversation: the agent that asked, else the seat itself.
  if (!named.length) named.push(...[item.requester_id, item.source_id].filter((id): id is string => !!id && id.startsWith('agent/')).slice(0, 1));
  return [...new Set(named)];
}

/** The alerts that show in one agent's conversation. */
export function alertsIn(items: readonly KeptAttention[], agentId: string, actor: string | undefined): KeptAttention[] {
  return openAlerts(items, actor).filter(item => alertConversations(item).includes(agentId));
}

/** A native prompt's answers, as its alert offers them; null when the alert offers none. */
export type PromptAnswers = { target_id: string; episode: string; answers: Array<'allow' | 'deny'> };
export function promptAnswers(item: Pick<Attention, 'actions' | 'source_id'> & { episode?: string; action_parameters?: unknown }): PromptAnswers | null {
  if (!(item.actions as readonly string[]).includes('prompt.respond')) return null;
  const parameters = (item.action_parameters as Record<string, unknown> | undefined)?.['prompt.respond'] as { target_id?: unknown; episode?: unknown; answers?: unknown } | undefined;
  if (!parameters) return null;
  const answers = (Array.isArray(parameters.answers) ? parameters.answers : []).filter((answer): answer is 'allow' | 'deny' => answer === 'allow' || answer === 'deny');
  if (!answers.length) return null;
  const target_id = typeof parameters.target_id === 'string' ? parameters.target_id : item.source_id;
  const episode = typeof parameters.episode === 'string' ? parameters.episode : item.episode ?? '';
  return episode ? { target_id, episode, answers } : null;
}

/** The call a harness recorded before it asked, in full: its tool and each input, lines intact. */
export type PendingCall = { id: string; tool: string; lines: string[] };

function inputLines(args: unknown): string[] {
  let value = args;
  if (typeof args === 'string') { try { value = JSON.parse(args); } catch { return args.split('\n'); } }
  if (value === null || value === undefined) return [];
  if (typeof value !== 'object' || Array.isArray(value)) return JSON.stringify(value, null, 2).split('\n');
  const lines: string[] = [];
  for (const [key, field] of Object.entries(value as Record<string, unknown>)) {
    const text = typeof field === 'string' ? field : JSON.stringify(field, null, 2);
    const [first = '', ...rest] = text.split('\n');
    lines.push(`${key}: ${first}`, ...rest.map(line => `  ${line}`));
  }
  return lines;
}

/**
 * Every tool call with no result yet, oldest first. A permission prompt asks about one of them,
 * but the prompt carries no call ID, so with more than one unanswered a client cannot say which.
 */
export function pendingCalls(entries: ReadonlyArray<Pick<TimelineEntry, 'type' | 'body' | 'id'>>): PendingCall[] {
  const answered = new Set<string>();
  for (const entry of entries) {
    if (entry.type === 'tool_result') { const call = (entry.body as { call_id?: unknown }).call_id; if (typeof call === 'string') answered.add(call); }
  }
  const open: PendingCall[] = [];
  for (const entry of entries) {
    if (entry.type !== 'tool_call') continue;
    const body = entry.body as { call_id?: unknown; name?: unknown; arguments?: unknown };
    const call = typeof body.call_id === 'string' ? body.call_id : entry.id;
    if (answered.has(call) || open.some(other => other.id === call)) continue;
    open.push({ id: call, tool: typeof body.name === 'string' && body.name ? body.name : 'tool', lines: inputLines(body.arguments) });
  }
  return open;
}

/**
 * The answers a client may offer: deny always; allow only when exactly one call is unanswered and
 * it is shown in full, since only then is it certain what an allow allows.
 */
export function offeredAnswers(prompt: PromptAnswers, calls: readonly PendingCall[]): Array<'allow' | 'deny'> {
  const certain = calls.length === 1 && calls[0].lines.length > 0;
  return prompt.answers.filter(answer => answer === 'deny' || certain);
}
