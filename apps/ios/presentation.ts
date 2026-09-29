import type { Agent, Attention, Device, Mission, MissionStep } from '../../clients/typescript/st3-client';

// The daemon annotates every resource with its operational layer. It is not part of the
// generated resource types, so read it at the UI boundary.
type Operational = { layer?: string; actionable?: boolean; reasons?: string[] };
function operational(resource: object): Operational | undefined {
  return (resource as { operational?: Operational }).operational;
}

const acronyms: Record<string, string> = { st3: 'ST', cos: 'COS', ios: 'iOS', tui: 'TUI', pty: 'PTY', api: 'API' };
export function words(slug: string): string {
  return slug.split('-').map(word => acronyms[word.toLowerCase()] ?? word.charAt(0).toUpperCase() + word.slice(1)).join(' ');
}
function titleCase(text: string): string { return text.charAt(0).toUpperCase() + text.slice(1); }

export function ago(iso: string, now = Date.now()): string {
  const at = Date.parse(iso);
  if (!Number.isFinite(at)) return 'unknown';
  const seconds = Math.max(0, Math.floor((now - at) / 1000));
  if (seconds < 60) return `${seconds}s`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m`;
  if (seconds < 86_400) return `${Math.floor(seconds / 3600)}h`;
  return `${Math.floor(seconds / 86_400)}d`;
}

export function attentionHeadline({ count, loaded, error }: { count: number; loaded: boolean; error?: string }): { text: string; warning: boolean } {
  if (error) return count ? { text: `${count} actionable items from the last load · refresh failed: ${error}`, warning: true } : { text: `Attention could not be loaded: ${error}`, warning: true };
  if (!loaded) return { text: 'Attention has not loaded yet.', warning: true };
  return { text: count ? `${count} actionable items` : 'Nothing needs your attention.', warning: false };
}

const actionLabels: Record<Attention['actions'][number], string> = {
  'attention.resolve': 'Resolve',
  'review.approve': 'Approve review',
  'review.reject': 'Reject review',
  'review.request-changes': 'Request changes',
  'launch.approve': 'Approve launch',
  'launch.cancel': 'Cancel launch',
  'mission.approve-revision': 'Approve revision',
  'mission.cancel-revision': 'Cancel revision',
  'message.read': 'Mark read',
};
export function attentionActionLabel(action: string): string {
  return actionLabels[action as keyof typeof actionLabels] ?? titleCase(action.replace(/[.-]/g, ' '));
}
// The server can introduce a kind before this generated client contract catches up.
const kindLabels: Record<string, string> = {
  'human-gate': 'Needs a decision',
  'launch-approval': 'Launch approval',
  'revision-approval': 'Revision approval',
  'unread-message': 'Unread message',
  'agent-request': 'Agent request',
  fault: 'Fault',
};
export function attentionKindLabel(kind: string): string {
  return kindLabels[kind as keyof typeof kindLabels] ?? titleCase(kind.replace(/-/g, ' '));
}

function leaf(path: string): string { return path.split('/').pop() || path; }

// st joins each agent's queue into its row: the step it works on, the next one, and the rest.
type QueueAgent = Pick<Agent, 'next_work' | 'next_work_id' | 'upcoming_work' | 'queued_work_count'>;
export function queuedWorkSummary(agent: QueueAgent, now = Date.now()): string | null {
  const next = agent.next_work ?? null;
  if (!next && !agent.next_work_id) return null;
  const queued = [...(next ? [next] : []), ...(agent.upcoming_work ?? []).filter(step => step.id !== next?.id)];
  // A ready step's `since` is when it became ready; that is how long it has waited.
  const oldest = queued.filter(step => step.state === 'ready').map(step => step.since).sort()[0];
  const name = next ? leaf(next.path) : leaf(agent.next_work_id!);
  return `Next: ${name} · ${agent.queued_work_count ?? queued.length} queued${oldest ? ` · oldest ready ${ago(oldest, now)}` : ''}`;
}

export function currentWorkSummary(agent: Pick<Agent, 'current_work'>): string | null {
  const current = agent.current_work?.[0];
  return current ? `Current: ${current.title || leaf(current.path)} (${current.state})` : null;
}

// A mission row carries its runs with their steps: every open run, and the latest run.
export function missionSteps(mission: Pick<Mission, 'run_details'>): MissionStep[] {
  return (mission.run_details ?? []).flatMap(run => run.steps ?? []);
}

export const missionGroups = ['Blocked', 'Waiting', 'Running', 'Drafts', 'Archive'] as const;
export type MissionGroup = typeof missionGroups[number];
export function missionGroup(mission: Pick<Mission, 'run_details' | 'state'>): MissionGroup {
  const states = missionSteps(mission).map(step => step.state);
  if (states.includes('blocked')) return 'Blocked';
  if (states.includes('waiting')) return 'Waiting';
  if (mission.state === 'running' || mission.state === 'standing') return 'Running';
  if (mission.state === 'ready' || mission.state === 'draft') return 'Drafts';
  return 'Archive';
}

export function missionDetail(mission: Pick<Mission, 'run_details' | 'runs' | 'visualization'>): string {
  const planned = mission.visualization?.nodes.filter(node => node.kind === 'step').length;
  const steps = missionSteps(mission);
  const current = steps.find(step => step.state === 'blocked') ?? steps.find(step => step.state === 'claimed' || step.state === 'ready') ?? steps[0];
  return `${mission.runs.length} runs${planned === undefined ? '' : ` · ${planned} planned steps`}${current ? ` · ${current.path} (${current.state})` : ''}`;
}

type HealthAgent = Pick<Agent, 'state' | 'harness_state'>;
export function agentHealth(agent: HealthAgent): { healthy: boolean; label: string } {
  const reasons = operational(agent)?.reasons ?? [];
  const healthy = agent.state === 'running' && reasons.length === 0;
  const harness = agent.harness_state ? (healthy ? agent.harness_state : `harness ${agent.harness_state}`) : null;
  return { healthy, label: [titleCase(agent.state), harness, ...reasons].filter(Boolean).join(' · ') };
}

export function agentHeaderDetail(agent: HealthAgent & Pick<Agent, 'driver' | 'updated_at'>, now = Date.now()): string {
  return `${agent.driver ?? 'harness'} · ${agentHealth(agent).label} · observed ${ago(agent.updated_at, now)} ago`;
}

type DeviceView = Pick<Device, 'id' | 'session_actor' | 'state' | 'updated_at' | 'expires_at'> & { name?: string | null };
export function deviceTitle(device: DeviceView, connectedActor: string | undefined): string {
  const mine = !!connectedActor && device.session_actor === connectedActor;
  const name = device.name?.trim();
  if (name) return mine ? `${name} · this device` : name;
  return mine ? 'This device' : `Paired device ${device.id.replace(/^device\//, '').slice(0, 8)}`;
}
export function deviceDetail(device: DeviceView, now = Date.now()): string {
  return `${device.state} · paired ${ago(device.updated_at, now)} ago · expires ${device.expires_at.slice(0, 10)}`;
}

// Mission titles are paths such as `fleet/app-apple/issue-triage`. The leaf is the readable name;
// when leaves collide, the project segment tells them apart.
export function missionLabels(missions: Array<Pick<Mission, 'id' | 'title'>>): Map<string, string> {
  const segments = new Map(missions.map(mission => [mission.id, mission.title.split('/').filter(part => part && part !== 'fleet' && part !== '__st3')]));
  const leaf = (parts: string[]) => words(parts.at(-1) ?? '');
  const counts = new Map<string, number>();
  for (const parts of segments.values()) counts.set(leaf(parts), (counts.get(leaf(parts)) ?? 0) + 1);
  const labels = new Map<string, string>();
  for (const mission of missions) {
    const parts = segments.get(mission.id)!;
    const base = leaf(parts) || mission.title;
    labels.set(mission.id, (counts.get(leaf(parts)) ?? 0) > 1 && parts.length > 1 ? `${base} · ${words(parts[0])}` : base);
  }
  // Anything still ambiguous (for example loop rounds) falls back to its distinguishing path.
  const seen = new Map<string, number>();
  for (const label of labels.values()) seen.set(label, (seen.get(label) ?? 0) + 1);
  for (const mission of missions) {
    const label = labels.get(mission.id)!;
    if ((seen.get(label) ?? 0) > 1) labels.set(mission.id, `${label} · ${segments.get(mission.id)!.slice(0, -1).join('/') || mission.id}`);
  }
  return labels;
}

function senderName(id: string): string {
  if (id.startsWith('person/')) return id.slice('person/'.length);
  const slug = id.split('/').filter(Boolean).pop() ?? id;
  return words(slug.split('.').pop() ?? slug);
}

// PING delivery lines are `[PING] <marker> <from>: <subject> [id:<reference>]`. The marker only
// describes a claimed relationship (`?` when unknown) and the reference is for agents, not people.
export function pingPresentation(text: string): { from: string | null; text: string } {
  const withoutReference = text.replace(/\s*\[id:[^\]\s]+\]\s*$/, '');
  const ping = /^\[PING\]\s+\S+\s+(\S+):\s+([\s\S]*)$/.exec(withoutReference);
  return ping ? { from: senderName(ping[1]), text: ping[2].trim() } : { from: null, text: withoutReference };
}

// st joins Small Talk to or from an agent into its conversation: who wrote to whom, and the title.
export function smallTalkPresentation(body: unknown): { from: string; text: string } {
  const message = (body && typeof body === 'object' ? body : {}) as { from?: unknown; to?: unknown; title?: unknown };
  const name = (id: unknown) => typeof id === 'string' ? senderName(id) : 'someone';
  return { from: `${name(message.from)} → ${name(message.to)}`, text: typeof message.title === 'string' && message.title.trim() ? message.title : 'Small Talk' };
}
