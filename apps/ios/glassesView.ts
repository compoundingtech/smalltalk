import type { Agent, Attention, Glass, GlassLayout, Mission } from '../../clients/typescript/st3-client';
import { agentName } from './agentsView';
import { missionLabels } from './presentation';
import type { MachineView } from './projectionCache';

// Glasses on the phone, an experiment behind a setting (mission fleet/stui/glass): the person's
// named workspaces from stui, read from st. The phone shows one thing at a time: a glass's tabs
// as a list, Home first, and a tab's panes one after another. Pane keys are stui's
// (crates/stui/src/ui/pane.rs): `agent:agent/…`, `mission:mission/…`, `machine:machine/…`.

/** What a pane opens on the phone. */
export type PaneTarget = {
  key: string;
  kind: 'agent' | 'mission' | 'machine' | 'other';
  /** The subject's id: `agent/…`, `mission/…`, `machine/…`; the key for anything else. */
  id: string;
  title: string;
  /** Something about this subject waits on the person. */
  needsYou: boolean;
  /** st no longer lists the subject. */
  gone: boolean;
};

export type GlassTabRow = {
  index: number;
  title: string;
  panes: PaneTarget[];
  needsYou: boolean;
};

export type GlassLists = {
  agents: Agent[];
  missions: Mission[];
  attention: Attention[];
  machines: MachineView[];
};

/** Every pane key in a layout, left to right and top to bottom. */
export function leaves(layout: GlassLayout): string[] {
  if ('pane' in layout) return [layout.pane];
  return layout.children.flatMap(leaves);
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

/** A glass's tabs after Home: titled by their own title or their first pane. */
export function glassTabs(glass: Glass, lists: GlassLists): GlassTabRow[] {
  const labels = missionLabels(lists.missions);
  return (glass.body?.tabs ?? []).map((tab, index) => {
    const panes = leaves(tab.layout).map(key => paneTarget(key, lists, labels));
    const first = panes[0]?.title ?? 'empty';
    const more = panes.length - 1;
    return {
      index,
      title: tab.title ?? (more > 0 ? `${first} +${more}` : first),
      panes,
      needsYou: panes.some(pane => pane.needsYou),
    };
  });
}
