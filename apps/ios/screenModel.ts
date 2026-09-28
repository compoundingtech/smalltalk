// What each screen lists and in what order, independent of how it is drawn. These follow
// stui's `ui::screens` so the terminal and the phone group, sort and explain the same way.

import type { Agent, Attention, Entry, Load, Mission, Tier, Word, World, Worktree } from './clientView';
import { items } from './clientView.ts';
import { agentRank, MISSION_WORDS, TIERS, wordRank } from './words.ts';

export type ListState = { kind: 'loading'; text: string } | { kind: 'empty'; text: string } | { kind: 'failed'; text: string } | { kind: 'ready' };

export function listState<T>(load: Load<T[]>, loadingText: string, emptyText: string): ListState {
  if (load.state === 'loading') return { kind: 'loading', text: loadingText };
  if (load.state === 'failed') return { kind: 'failed', text: load.value };
  return load.value.length ? { kind: 'ready' } : { kind: 'empty', text: emptyText };
}

export type Section<T> = { key: string; title: string; count: number; items: T[] };

function sections<T>(sorted: T[], group: (item: T) => { key: string; title: string }): Section<T>[] {
  const out: Section<T>[] = [];
  for (const item of sorted) {
    const { key, title } = group(item);
    const last = out[out.length - 1];
    if (last?.key === key) { last.items.push(item); last.count++; } else out.push({ key, title, count: 1, items: [item] });
  }
  return out;
}

const compare = (a: string | number | boolean, b: string | number | boolean) => (a < b ? -1 : a > b ? 1 : 0);

// ---------------------------------------------------------------------- home

export function homeSections(world: World, snoozed: ReadonlySet<string>): Section<Attention>[] {
  const rank = (tier: Tier) => TIERS.findIndex(info => info.id === tier);
  const shown = items(world.attention).filter(item => !snoozed.has(item.id)).sort((a, b) => rank(a.tier) - rank(b.tier));
  return sections(shown, item => ({ key: item.tier, title: TIERS[rank(item.tier)].title }));
}

/** The Home badge: every open item, in the person colour when someone is stopped on you. */
export function homeBadge(world: World): { count: number; stopped: boolean } {
  const all = items(world.attention);
  return { count: all.length, stopped: all.some(item => item.tier === 'stopped') };
}

export const HOME_TEXT = { loading: 'Checking what needs you…', empty: 'Nothing needs you.' };

// --------------------------------------------------------------------- agents

export function agentOrder(world: World): Agent[] {
  return [...items(world.agents)].sort((a, b) =>
    compare(a.unmanaged, b.unmanaged) || agentRank(a.state) - agentRank(b.state) || compare(a.name.toLowerCase(), b.name.toLowerCase()));
}

export function agentGroup(agent: Agent): string {
  if (agent.unmanaged) return 'found running · not started by st';
  switch (agent.state) {
    case 'needs_you': return 'waiting on you';
    case 'fault': return 'broken';
    case 'working': return 'working';
    case 'idle': case 'starting': return 'idle';
    case 'stopped': case 'unknown': return 'stopped';
  }
}

export function agentSections(world: World): Section<Agent>[] {
  return sections(agentOrder(world), agent => ({ key: agentGroup(agent), title: agentGroup(agent) }));
}

export const agentPath = (agent: Agent) => (agent.id.startsWith('agent/') ? agent.id.slice('agent/'.length) : agent.id);

// ------------------------------------------------------------------- missions

export function missionOrder(world: World, system: boolean): Mission[] {
  return items(world.missions).filter(mission => system || !mission.system)
    .sort((a, b) => wordRank(a.word) - wordRank(b.word) || compare(a.title.toLowerCase(), b.title.toLowerCase()));
}

export function missionSections(world: World, system: boolean): Section<Mission>[] {
  return sections(missionOrder(world, system), mission => ({ key: mission.word, title: MISSION_WORDS[wordRank(mission.word)].name }));
}

export const hiddenSystemMissions = (world: World, system: boolean) => (system ? 0 : items(world.missions).filter(mission => mission.system).length);

export const missionPath = (mission: Mission) => (mission.id.startsWith('mission/') ? mission.id.slice('mission/'.length) : mission.id);

// --------------------------------------------------------------------- trees

export type TreeRow = { kind: 'folder'; depth: number; name: string; key: string } | { kind: 'leaf'; depth: number; id: string; name: string };

/** A list laid out as the graph's path tree: folders from the id, one line per leaf. */
export function pathTree(leaves: { path: string[]; id: string }[]): TreeRow[] {
  const sorted = [...leaves].sort((a, b) => {
    for (let index = 0; index < Math.min(a.path.length, b.path.length); index++) {
      const order = compare(a.path[index], b.path[index]);
      if (order) return order;
    }
    return a.path.length - b.path.length;
  });
  const rows: TreeRow[] = [];
  const open: string[] = [];
  for (const { path, id } of sorted) {
    const folders = path.slice(0, -1);
    let shared = 0;
    while (shared < open.length && shared < folders.length && open[shared] === folders[shared]) shared++;
    open.length = shared;
    for (let depth = shared; depth < folders.length; depth++) {
      rows.push({ kind: 'folder', depth, name: `${folders[depth]}/`, key: folders.slice(0, depth + 1).join('/') });
      open.push(folders[depth]);
    }
    rows.push({ kind: 'leaf', depth: folders.length, id, name: path[path.length - 1] ?? id });
  }
  return rows;
}

export const agentsTree = (world: World) => pathTree(agentOrder(world).map(agent => ({ path: agentPath(agent).split('/'), id: agent.id })));
export const missionsTree = (world: World, system: boolean) => pathTree(missionOrder(world, system).map(mission => ({ path: missionPath(mission).split('/'), id: mission.id })));

// ---------------------------------------------------------------------- flow

/** Steps as layers: each after the ones it depends on, e.g. `scan → fix → {review, lint}`. */
export function flowLayers<T extends { name: string; after: string[] }>(steps: T[]): T[][] {
  const depth = steps.map(() => 0);
  for (let round = 0; round < steps.length; round++) {
    steps.forEach((step, index) => {
      for (const dependency of step.after) {
        const position = steps.findIndex(candidate => candidate.name === dependency);
        if (position !== -1) depth[index] = Math.max(depth[index], depth[position] + 1);
      }
    });
  }
  const layers = steps.length ? Math.max(...depth) + 1 : 0;
  return Array.from({ length: layers }, (_, layer) => steps.filter((_, index) => depth[index] === layer));
}

// ---------------------------------------------------------------- chat about

/** Who to talk to about an item: the agent involved, or the chief of staff. */
export function chatTarget(world: World, item: Attention): { id: string; name: string } | null {
  const agents = items(world.agents);
  const named = item.agent ? agents.find(agent => agent.id === item.agent) : undefined;
  const target = named ?? agents.find(agent => agent.id.endsWith('/cos') || agent.name === 'Chief of Staff');
  return target ? { id: target.id, name: target.name } : null;
}

export const aboutTitle = (item: Attention) => `About: ${item.title}`;

/** A "chat about this" message: the person's words, then the item as context. */
export function aboutText(item: Attention, text: string): string {
  return `${text}\n\n---\nThis is about ${item.title} (${item.id}${item.mission ? `, mission ${item.mission}` : ''})`;
}

/** The thread a "chat about this" started, from the agent's conversation. */
export function aboutThread(world: World, to: string, item: Attention): Entry[] {
  const conversation = world.conversations[to];
  if (conversation?.state !== 'ready') return [];
  const title = aboutTitle(item);
  return conversation.value.filter(entry => entry.body.kind === 'mail' && entry.body.value.subject === title);
}

/** Go to what an item is about: its mission, or else its agent. */
export const goToSubject = (item: Attention) => item.mission ?? item.agent;

// ---------------------------------------------------------------- missions help

export type Help =
  | { kind: 'none' }
  | { kind: 'queued'; note: string }
  | { kind: 'stuck'; word: Word; step: Mission['steps'][number] | null; broken: Agent | null };

/** For a mission that is not moving: why, and what a person can do about it. */
export function missionHelp(world: World, mission: Mission): Help {
  const stuck = mission.steps.find(step => step.state === 'failed' || step.state === 'ready' || step.state === 'waiting') ?? null;
  if (mission.word === 'queued') return { kind: 'queued', note: stuck?.note ?? 'It is waiting its turn.' };
  if (mission.word !== 'stalled' && mission.word !== 'failed' && mission.word !== 'unstaffed') return { kind: 'none' };
  const broken = mission.agents.map(id => items(world.agents).find(agent => agent.id === id))
    .find((agent): agent is Agent => agent !== undefined && (agent.state === 'fault' || agent.state === 'stopped')) ?? null;
  return { kind: 'stuck', word: mission.word, step: stuck, broken };
}

// ----------------------------------------------------------------- worktrees

export function worktreeSections(world: World): Section<Worktree>[] {
  const sorted = [...items(world.worktrees)].sort((a, b) => compare(a.host, b.host) || compare(a.path, b.path));
  return sections(sorted, tree => ({ key: tree.host, title: `on ${tree.host}` }));
}
export const worktreeId = (tree: Worktree) => `${tree.host}:${tree.path}`;

// ------------------------------------------------------------- pending sends

/** A message sent from here that st has not reported back yet. */
export type PendingSend = { token: string; agent: string; text: string; at: string; messageId: string | null; failed: string | null };

/** A pending copy is done once st reports its message id among the conversation's messages. */
export function reconcilePending(pending: PendingSend[], agent: string, messageIds: ReadonlySet<string>): PendingSend[] {
  return pending.filter(entry => entry.agent !== agent || entry.messageId === null || !messageIds.has(entry.messageId));
}

/** Conversations with this device's pending sends at the end, dim until st has them. */
export function withPending(conversations: Record<string, Load<Entry[]>>, pending: PendingSend[]): Record<string, Load<Entry[]>> {
  const out = { ...conversations };
  for (const entry of pending) {
    const current = out[entry.agent];
    if (current?.state !== 'ready') continue;
    out[entry.agent] = { state: 'ready', value: [...current.value, { id: `pending:${entry.token}`, at: entry.at, body: { kind: 'pending', value: { text: entry.text, failed: entry.failed } } }] };
  }
  return out;
}
