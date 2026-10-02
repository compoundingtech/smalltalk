import type { Agent, Attention, Glass, GlassLayout, GlassTab, Mission } from '../../clients/typescript/st3-client';
import { agentName } from './agentsView';
import { missionLabels } from './presentation';
import type { MachineView } from './projectionCache';

// Glasses on the phone, an experiment behind a setting (mission fleet/stui/glass): the person's
// named workspaces from stui, read from st. A glass is splits, each with its own tabs, and each
// tab one pane. The phone shows one thing at a time: Home, then each split's tabs as a list, and
// a tab opens its pane full screen. Pane keys are stui's
// (crates/stui/src/ui/pane.rs): `agent:agent/…`, `mission:mission/…`, `machine:machine/…`.

/** What a pane opens on the phone. */
export type PaneTarget = {
  key: string;
  kind: 'agent' | 'mission' | 'machine' | 'terminal' | 'usage' | 'other';
  /** The subject's id: `agent/…`, `mission/…`, `machine/…`; the key for anything else. */
  id: string;
  title: string;
  /** Something about this subject waits on the person. */
  needsYou: boolean;
  /** st no longer lists the subject. */
  gone: boolean;
};

export type GlassTabRow = {
  /** Its split, counted left to right and top to bottom, as stui counts them. */
  group: number;
  index: number;
  title: string;
  pane: PaneTarget;
};

export type GlassGroupRow = {
  index: number;
  tabs: GlassTabRow[];
};

export type GlassLists = {
  agents: Agent[];
  missions: Mission[];
  attention: Attention[];
  machines: MachineView[];
};

/** Every split's tabs, left to right and top to bottom. */
export function groups(layout: GlassLayout): GlassTab[][] {
  if ('tabs' in layout) return [layout.tabs];
  return layout.children.flatMap(groups);
}

/** The glasses to choose between, by name, live ones only. */
export function glassChoices(glasses: Glass[]): Array<{ id: string; name: string }> {
  return glasses
    .filter(glass => !glass.deleted && glass.body)
    .map(glass => ({ id: glass.id, name: glass.body!.name }))
    .sort((a, b) => a.name.localeCompare(b.name) || a.id.localeCompare(b.id));
}

const open = (attention: Attention[]) => attention.filter(item => item.state !== 'resolved');

/** What one pane key opens, named as the rest of the app names it. `labels` are the missions'
 * short names (`missionLabels`), worked out once for every pane. */
export function paneTarget(key: string, lists: GlassLists, labels = missionLabels(lists.missions)): PaneTarget {
  const split = key.indexOf(':');
  const kind = split < 0 ? '' : key.slice(0, split);
  const id = split < 0 ? key : key.slice(split + 1);
  const waiting = open(lists.attention);
  // A shell of its own (`terminal:terminal/…`) opens in the terminal; an agent's terminal opens
  // the agent, whose terminal is a tap away there.
  if (kind === 'terminal' && id.startsWith('terminal/')) {
    return { key, kind: 'terminal', id, title: 'shell', needsYou: false, gone: false };
  }
  if (kind === 'usage') {
    return { key, kind: 'usage', id, title: 'Usage', needsYou: false, gone: false };
  }
  if (kind === 'agent' || kind === 'terminal') {
    const agent = lists.agents.find(candidate => candidate.id === id);
    return {
      key, kind: 'agent', id,
      title: agent ? agentName(agent) : id,
      needsYou: waiting.some(item => item.requester_id === id || item.source_id === id),
      gone: !agent,
    };
  }
  if (kind === 'mission' || kind === 'declaration') {
    const mission = lists.missions.find(candidate => candidate.id === id);
    return {
      key, kind: 'mission', id,
      title: mission ? (labels.get(id) ?? mission.title) : id,
      needsYou: waiting.some(item => item.mission_id === id),
      gone: !mission,
    };
  }
  if (kind === 'machine') {
    const name = id.replace(/^machine\//, '');
    const machine = lists.machines.find(candidate => candidate.name === name || candidate.id === id);
    // Machines load when Fleet opens, so an unknown one is not yet a gone one.
    return { key, kind: 'machine', id, title: name, needsYou: false, gone: lists.machines.length > 0 && !machine };
  }
  return { key, kind: 'other', id: key, title: key, needsYou: false, gone: false };
}

/** A glass's splits and their tabs, after Home: each tab titled by its own title or its pane. */
export function glassGroups(glass: Glass, lists: GlassLists): GlassGroupRow[] {
  const labels = missionLabels(lists.missions);
  const layout = glass.body?.layout ?? { tabs: [] };
  return groups(layout).map((tabs, group) => ({
    index: group,
    tabs: tabs.map((tab, index) => {
      const pane = paneTarget(tab.pane, lists, labels);
      return { group, index, title: tab.title ?? pane.title, pane };
    }),
  }));
}

/** A space as the list names it: how many tabs it holds and in how many panes. */
export function spaceSummary(glass: Glass): string {
  const panes = groups(glass.body?.layout ?? { tabs: [] });
  const tabs = panes.reduce((sum, tabs) => sum + tabs.length, 0);
  return `${tabs} tab${tabs === 1 ? '' : 's'}${panes.length > 1 ? ` in ${panes.length} panes` : ''}`;
}
