// Live st3 data, turned into the view model the screens draw: the port of stui's `ui::adapt`.
//
// A collection that has not loaded yet becomes `loading`, never an empty list, and one that
// failed says why. Anything the graph does not say stays unsaid.

import type { Agent as GraphAgent, Attention as GraphAttention, Mission as GraphMission, Runtime, Work } from '../../clients/typescript/st3-client';
import type { Agent, AgentState, Attention, AttentionKind, Entry, Harness, Load, Machine, Mission, MissionPreview, PreviewAgent, PreviewStep, Step, StepState, Tier, Word, World, Worktree } from './clientView';
import { emptyDetails } from './clientView.ts';
import { short, trimPrefix } from './harnessConversation.ts';
import { cleanMessageText, lines, trim } from './messageText.ts';

export type GraphMachine = {
  id: string;
  name: string;
  host_id?: string;
  state: string;
  updated_at?: string;
  occupancy: { running_runtimes: number };
  transports: Array<{ protocol: string; status: string }>;
};
export type GraphSession = { id: string; kind: 'session'; owner_id: string; state: string; updated_at: string; managed?: boolean; driver?: string; workspace?: string };

/** One collection as the live client holds it: not loaded yet, loaded, or failed. */
export type Collection<T> = { items: T[]; loaded: boolean; error?: string };
export const notLoaded = <T>(): Collection<T> => ({ items: [], loaded: false });

export type Graph = {
  /** The paired person, e.g. `person/robin`. */
  actor: string;
  /** The gateway's host, e.g. `host/lark`, once a read has said. */
  hostId: string | null;
  attention: Collection<GraphAttention>;
  agents: Collection<GraphAgent>;
  missions: Collection<GraphMission>;
  work: Collection<Work>;
  machines: Collection<GraphMachine>;
  runtimes: Collection<Runtime>;
  sessions: Collection<GraphSession>;
};

/** A message behind an unread-message item: sender, title and text. */
export type MessageBody = { from: string; title: string | null; content: string };

/** What the live client fetched beside the graph: conversations and launch previews. */
export type Extras = {
  conversations: Record<string, Load<Entry[]>>;
  previews: Record<string, Load<MissionPreview>>;
  bodies: Record<string, MessageBody>;
  live: boolean;
  offline: string | null;
  worktrees: Load<Worktree[]>;
};

// ------------------------------------------------------------------- labels

function capitalize(word: string): string {
  const [first, ...rest] = Array.from(word);
  return first === undefined ? '' : first.toUpperCase() + rest.join('');
}
function titleWords(slug: string, special: Record<string, string>): string {
  return slug.split('-').map(word => special[word.toLowerCase()] ?? capitalize(word)).join(' ');
}
const lastSegment = (path: string) => path.split('/').pop() ?? path;

export function agentLabel(agent: { name: string }): string {
  const special = { st3: 'ST', cos: 'COS', ios: 'iOS', tui: 'TUI', pty: 'PTY', omp: 'OMP' };
  const segments = agent.name.split('/');
  const slug = segments[segments.length - 1];
  if (slug.toLowerCase() === 'omp' && segments.length > 1) return `${titleWords(segments[segments.length - 2], special)} · OMP`;
  return titleWords(slug, special);
}

export function missionLabel(mission: { title: string }): string {
  return titleWords(lastSegment(mission.title), { tui: 'TUI', ios: 'iOS', st3: 'ST', omp: 'OMP', api: 'API', pty: 'PTY' });
}

export function missionDisplayLabel(mission: { id: string; title: string }): string {
  const path = mission.id.startsWith('mission/') ? mission.id.slice('mission/'.length) : mission.id;
  const cut = path.lastIndexOf('/');
  return `${cut === -1 ? path : path.slice(0, cut)} · ${missionLabel(mission)}`;
}

/** "33m", "2h", "4d": how long ago, without the "ago". */
export function age(then: string | undefined, now: number): string {
  const parsed = then ? Date.parse(then) : NaN;
  if (Number.isNaN(parsed)) return 'unknown age';
  const seconds = Math.max(0, Math.floor((now - parsed) / 1000));
  if (seconds < 60) return `${seconds}s`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)}h`;
  return `${Math.floor(seconds / 86400)}d`;
}

export function missionWorkMatches(mission: GraphMission, work: Work): boolean {
  if (mission.runs[mission.runs.length - 1] !== work.mission_run_id) return false;
  const current = mission.run_generations[work.mission_run_id];
  return current === undefined || current === work.generation_id;
}

const isAgentless = (work: Work) => (work as Work & { agentless?: boolean }).agentless === true;

function workOwner(graph: Graph, work: Work): string {
  if (isAgentless(work)) return 'Agentless step';
  const agents = graph.agents.items;
  const id = work.claimant ?? agents.find(agent => agent.next_work_id === work.id || (agent.current_work_ids ?? []).includes(work.id))?.id;
  if (!id) return 'Unassigned';
  const agent = agents.find(candidate => candidate.id === id);
  return agent ? `${agentLabel(agent)} · ${id}` : id;
}

/** "Mission › step" for a work id, or the id itself when st has not sent that work. */
function stepLabel(graph: Graph, id: string): string {
  const work = graph.work.items.find(candidate => candidate.id === id);
  if (!work) return trimPrefix(id, 'step-run/');
  const mission = graph.missions.items.find(candidate => candidate.runs.includes(work.mission_run_id));
  return `${mission ? missionDisplayLabel(mission) : trimPrefix(work.mission_run_id, 'mission-run/')} › ${work.path}`;
}

/** The agent whose queue holds this work, if st says. */
function queuedFor(graph: Graph, work: string): GraphAgent | undefined {
  return graph.agents.items.find(agent => agent.next_work_id === work || (agent.upcoming_work_ids ?? []).includes(work));
}

/** Whether an agentless step only holds its run open (so the run's observers keep watching). */
export function keepsOpen(path: string): boolean {
  const name = lastSegment(path);
  return ['keep-watch', 'retire', 'steward-intake', 'standing', 'keep-open'].includes(name) || name.endsWith('-retirement');
}

const hostName = (id: string) => trimPrefix(id, 'host/');

function loaded<T>(collection: Collection<unknown>, value: T): Load<T> {
  if (collection.loaded) return { state: 'ready', value };
  if (collection.error) return { state: 'failed', value: collection.error };
  return { state: 'loading' };
}

// ------------------------------------------------------------------- the world

export function world(graph: Graph, extras: Extras, now: number): World {
  const link = extras.offline !== null ? { offline: extras.offline } : extras.live ? 'live' as const : 'connecting' as const;
  const missionsView = missions(graph, now);
  const quiet = missionsView.filter(mission => mission.word !== 'decision' && mission.word !== 'done' && !mission.system).length;
  const missionsCollection: Collection<unknown> = { items: [], loaded: graph.missions.loaded && graph.work.loaded, error: graph.missions.error ?? graph.work.error };
  return {
    person: graph.actor,
    host: graph.hostId ? hostName(graph.hostId) : 'this machine',
    link,
    attention: loaded(graph.attention, attention(graph, extras, now)),
    agents: loaded(graph.agents, agents(graph, now)),
    missions: loaded(missionsCollection, missionsView),
    machines: loaded(graph.machines, machines(graph, now)),
    worktrees: extras.worktrees,
    conversations: extras.conversations,
    quiet_missions: quiet,
  };
}

// ------------------------------------------------------------------- attention

export function openAttention(graph: Graph): GraphAttention[] {
  return graph.attention.items.filter(item => item.state !== 'resolved' && item.person_id === graph.actor);
}

function attention(graph: Graph, extras: Extras, now: number): Attention[] {
  return openAttention(graph).map(item => {
    const stepWork = item.step_run_id ? graph.work.items.find(work => work.id === item.step_run_id) : undefined;
    const step = stepWork?.path ?? null;
    const mission = item.mission_id ?? null;
    const targets = item.targets ?? [];
    const states = item.target_states ?? [];
    let tier: Tier;
    let kind: AttentionKind;
    const body = extras.bodies[item.id];
    switch (item.attention_kind) {
      case 'human-gate':
        tier = 'stopped';
        kind = {
          kind: 'review',
          question: item.detail,
          because: 'the step cannot finish until you answer',
          look_at: [
            ...states.map((target): [string, string] => [target.id, target.state]),
            ...targets.filter(target => !states.some(state => state.id === target)).map((target): [string, string] => ['review', target]),
          ],
          step: step ?? '',
        };
        break;
      case 'launch-approval':
        tier = 'today';
        kind = { kind: 'launch', planner: 'The planner', name: mission ? short(mission) : item.title, preview: extras.previews[item.id] ?? { state: 'loading' } };
        break;
      case 'revision-approval':
        tier = 'today';
        kind = { kind: 'revision', reason: item.detail, changes: [] };
        break;
      case 'unread-message':
        tier = 'later';
        kind = body
          ? { kind: 'message', from: body.from, body: body.title ? `**${cleanMessageText(body.title)}**\n\n${cleanMessageText(body.content)}` : cleanMessageText(body.content) }
          : { kind: 'message', from: item.detail.startsWith('Unread message from ') ? item.detail.slice('Unread message from '.length).replace(/\.+$/, '') : item.source_id, body: 'Loading the message…' };
        break;
      default:
        tier = item.priority === 'critical' || item.priority === 'high' ? 'alert' : 'today';
        kind = {
          kind: 'fault',
          what: item.detail,
          because: item.attention_kind === 'agent-request' ? 'an agent is asking you for help'
            : item.priority === 'critical' ? 'marked critical'
              : item.priority === 'high' ? 'marked high priority' : 'raised for you',
          fix: null,
          source: item.source_id === item.id ? '' : item.source_id,
        };
    }
    const graphMission = mission ? graph.missions.items.find(candidate => candidate.id === mission) : undefined;
    const agent = stepWork?.claimant
      // The agents working in the mission, for a gate that has no claimant.
      ?? (graphMission ? graph.agents.items.find(candidate => (candidate.current_work_ids ?? []).some(id => graph.work.items.some(work => work.id === id && missionWorkMatches(graphMission, work))))?.id : undefined)
      ?? (item.source_id.startsWith('agent/') ? item.source_id : undefined)
      ?? (kind.kind === 'message' && kind.from.startsWith('agent/') ? kind.from : undefined)
      ?? null;
    const related = targets.filter(target => target !== item.id).map((target): [string, string | null] => [target, states.find(state => state.id === target)?.state ?? null]);
    const extra = item as GraphAttention & { actor?: unknown };
    const raisedBy = typeof extra.requester_id === 'string' ? extra.requester_id : typeof extra.actor === 'string' ? extra.actor : null;
    const bodyTitle = body ? (body.title || lines(body.content).find(line => trim(line) !== '')) : undefined;
    return {
      id: item.id,
      tier,
      title: cleanMessageText(bodyTitle ?? item.title),
      waiting: step === null ? null : `step ${step}`,
      age: age(item.requested_at, now),
      mission,
      agent,
      kind,
      actions: [...item.actions],
      related,
      raised_by: raisedBy,
    };
  });
}

/** A launch preview from a launch variant's normalized mission. */
export function preview(name: string, normalized: Record<string, unknown>): MissionPreview {
  const strings = (value: unknown) => (Array.isArray(value) ? value.filter((item): item is string => typeof item === 'string').map(cleanMessageText) : []);
  const order = Array.isArray(normalized.display_order) ? normalized.display_order.filter((item): item is string => typeof item === 'string') : [];
  const stepsValue = normalized.steps && typeof normalized.steps === 'object' && !Array.isArray(normalized.steps) ? normalized.steps as Record<string, Record<string, unknown>> : {};
  const keys = order.filter(key => key in stepsValue);
  // serde_json maps iterate in key order.
  for (const key of Object.keys(stepsValue).sort()) if (!keys.includes(key)) keys.push(key);
  const agents: PreviewAgent[] = [];
  const steps: PreviewStep[] = [];
  for (const key of keys) {
    const step = stepsValue[key] ?? {};
    if (step.finally === true) continue;
    const selector = (step.work_selector ?? {}) as Record<string, unknown>;
    const agentId = typeof selector.agent === 'string' ? selector.agent : undefined;
    const assignee = selector.kind === 'assigned' ? (agentId ? short(agentId) : '')
      : selector.kind === 'available' ? 'any of several'
        : selector.kind === 'agentless' ? 'st' : '—';
    if (agentId && !agents.some(known => known.name === short(agentId))) agents.push({ name: short(agentId), harness: 'unknown', host: '' });
    const after = (Array.isArray(step.dependencies) ? step.dependencies : [])
      .map(dependency => (dependency as Record<string, unknown>)?.step).filter((dependency): dependency is string => typeof dependency === 'string');
    const asksYou = Array.isArray(step.gates) && step.gates.some(gate => gate && typeof gate === 'object' && 'reviewer' in gate);
    steps.push({ name: typeof step.path === 'string' ? step.path : key, assignee, after, asks_you: asksYou });
  }
  return { name, goals: strings(normalized.goals), steps, agents, workspace: '' };
}

// --------------------------------------------------------------------- agents

export function harness(driver: string | null | undefined): Harness {
  const name = driver ?? '';
  if (name.includes('claude')) return 'claude';
  if (name.includes('codex')) return 'codex';
  if (name.includes('omp')) return 'omp';
  if (name === 'pi') return 'pi';
  return 'unknown';
}

function agentState(agent: GraphAgent): AgentState {
  if (agent.fault) return 'fault';
  switch (agent.state) {
    case 'failed': return 'fault';
    case 'running': return agent.harness_state === 'working' ? 'working' : 'idle';
    case 'waiting': return agent.harness_state === 'unauthenticated' || agent.harness_state === 'blocked' ? 'needs_you' : 'starting';
    case 'starting': case 'desired': return 'starting';
    case 'stopped': return 'stopped';
    default: return 'unknown';
  }
}

function agents(graph: Graph, now: number): Agent[] {
  const declared = graph.agents.items.map((agent): Agent => {
    const runtime = graph.runtimes.items.find(candidate => candidate.owner_id === agent.id);
    const first = (agent.current_work_ids ?? [])[0];
    const work = first ? graph.work.items.find(candidate => candidate.id === first) : undefined;
    const mission = work ? graph.missions.items.find(candidate => candidate.runs.includes(work.mission_run_id))?.id ?? null : null;
    const parent = (agent.under ?? [])[0]?.agent_id ?? null;
    const parentAgent = parent ? graph.agents.items.find(candidate => candidate.id === parent) : undefined;
    return {
      id: agent.id,
      name: agentLabel(agent),
      harness: harness(agent.driver),
      state: agentState(agent),
      host: runtime ? hostName(runtime.owner_host_id) : '?',
      worktree: null,
      mission,
      step: work?.path ?? null,
      activity: age(agent.updated_at, now),
      unmanaged: false,
      parent,
      details: {
        goal: work?.goals[0] !== undefined ? cleanMessageText(work.goals[0]) : null,
        claimed: work ? `${age(work.updated_at, now)} ago` : null,
        next: agent.next_work_id ? stepLabel(graph, agent.next_work_id) : null,
        queue: (agent.upcoming_work_ids ?? []).map(id => stepLabel(graph, id)),
        queued: agent.queued_work_count ?? 0,
        harness_state: agent.harness_state ?? null,
        runtime: runtime?.state ?? null,
        fault: agent.fault ?? null,
        under: parent ? (parentAgent ? agentLabel(parentAgent) : short(parent)) : null,
      },
    };
  });
  const gateway = graph.hostId ? hostName(graph.hostId) : '';
  const undeclared = graph.sessions.items.filter(session => session.state === 'running' && session.managed === false).map((session): Agent => {
    const workspace = typeof session.workspace === 'string' ? session.workspace : null;
    return {
      id: session.id,
      name: `${session.driver ?? 'harness'} in ${workspace ? lastSegment(workspace) : '?'}`,
      harness: harness(session.driver),
      state: 'unknown',
      host: gateway,
      worktree: workspace,
      mission: null,
      step: null,
      activity: age(session.updated_at, now),
      unmanaged: true,
      parent: null,
      details: emptyDetails(),
    };
  });
  return [...declared, ...undeclared];
}

// ------------------------------------------------------------------- missions

function missions(graph: Graph, now: number): Mission[] {
  return graph.missions.items.map((mission): Mission => {
    const work = graph.work.items.filter(candidate => missionWorkMatches(mission, candidate));
    const decision = openAttention(graph).find(item => item.attention_kind === 'human-gate' && item.mission_id === mission.id)?.id ?? null;
    const states = work.map(step => step.state as string);
    const readyUnclaimed = work.find(step => step.state === 'ready' && !step.claimant);
    let word: Word;
    if (decision) word = 'decision';
    else if ((mission.state as string) === 'blocked' || states.includes('blocked')) word = 'stalled';
    else if (states.includes('failed')) word = 'failed';
    else if (work.length > 0
      && work.every(step => step.state === 'completed' || (['claimed', 'running'].includes(step.state) && workOwner(graph, step) === 'Agentless step' && keepsOpen(step.path)))
      && work.some(step => step.state !== 'completed')) word = 'watching';
    else if (states.some(state => state === 'claimed' || state === 'running')) word = 'working';
    else if (readyUnclaimed) {
      const agent = queuedFor(graph, readyUnclaimed.id);
      word = !agent ? 'unclaimed' : agent.state === 'failed' || agent.state === 'stopped' || agent.fault ? 'unstaffed' : 'queued';
    } else if (states.includes('waiting')) word = 'held';
    else if (['standing', 'running', 'ready', 'draft'].includes(mission.state)) word = 'idle';
    else word = 'done';
    const steps = work.map((step): Step => {
      const state: StepState = step.state === 'completed' ? 'done'
        : step.state === 'claimed' || (step.state as string) === 'running' ? 'working'
          : step.state === 'ready' ? 'ready'
            : step.state === 'waiting' || step.state === 'blocked' ? 'waiting'
              : step.state === 'failed' ? 'failed' : 'pending';
      const owner = workOwner(graph, step);
      let note: string | null = null;
      if (step.state === 'ready' && !step.claimant) {
        const agent = queuedFor(graph, step.id);
        if (agent) {
          const current = (agent.current_work_ids ?? [])[0];
          note = current && agent.state !== 'failed' && agent.state !== 'stopped'
            ? `queued for ${agentLabel(agent)}, which is busy with ${stepLabel(graph, current)}`
            : `queued for ${agentLabel(agent)}, which is ${agent.state}`;
        }
      }
      return {
        name: step.path,
        state,
        owner: owner === '' || owner === 'unassigned' ? null : owner === 'Agentless step' ? 'st' : owner,
        note: note ?? step.blocked_reason ?? null,
        after: [],
        age: age(step.updated_at, now),
        goals: step.goals.map(goal => cleanMessageText(goal)),
        constraints: step.constraints.map(constraint => cleanMessageText(constraint)),
        gates: [],
        attempt: step.attempt,
        blockers: [...(step.blockers ?? [])],
      };
    });
    return {
      id: mission.id,
      title: missionDisplayLabel(mission),
      word,
      age: age(mission.updated_at, now),
      host: '',
      goals: [],
      steps,
      agents: graph.agents.items.filter(agent => (agent.current_work_ids ?? []).some(id => work.some(step => step.id === id))).map(agent => agent.id),
      decision,
      worktree: null,
      parent: null,
      system: mission.id.startsWith('mission/__st3/'),
      kdl: null,
    };
  });
}

// ------------------------------------------------------------------- machines

function machines(graph: Graph, now: number): Machine[] {
  return graph.machines.items.map(machine => ({
    name: machine.name,
    online: ['online', 'reachable', 'local', 'active'].includes(machine.state),
    platform: machine.state,
    seen: age(machine.updated_at, now),
    load: `${machine.occupancy.running_runtimes} running runtimes`,
    links: machine.transports.map((transport): [string, boolean, string] => [transport.protocol, ['ok', 'connected', 'reachable', 'healthy'].includes(transport.status), transport.status]),
    you_are_here: graph.hostId !== null && machine.host_id === graph.hostId,
  }));
}

// -------------------------------------------------------------- conversations

/** Display names for the ids that appear in message headers. */
export function names(graph: Graph): Record<string, string> {
  const out: Record<string, string> = {};
  for (const agent of graph.agents.items) out[agent.id] = agentLabel(agent);
  out[graph.actor] = 'you';
  return out;
}

/** The session whose transcript is this agent's conversation. */
export function sessionFor(graph: Graph, agent: string): string | null {
  if (agent.startsWith('session/')) return agent;
  const declared = graph.agents.items.find(candidate => candidate.id === agent);
  if (!declared) return null;
  return declared.current_session_id ?? graph.sessions.items.find(session => session.owner_id === agent && session.state === 'running')?.id ?? null;
}
