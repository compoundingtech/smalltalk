import type { Agent, Mission } from '../../clients/typescript/st3-client';
import { ago } from './presentation';
import type { SessionView } from '@smalltalk/st3-views';
import { harnessColor, theme } from './theme';

// The Agents tab, drawn the way stui draws it (crates/stui/src/ui/screens.rs agents_list and
// agents_tree, adapt.rs agents): one row per seat, grouped by what it needs, with the sessions
// st found running but did not start listed last.

// Declaration order is the sort order, as in stui's AgentState.
export const AGENT_STATES = ['needs-you', 'needs-login', 'fault', 'working', 'idle', 'starting', 'stopped', 'unknown'] as const;
export type AgentState = typeof AGENT_STATES[number];

export type AgentRowView = {
  /** The agent id, or the session id of an undeclared session. */
  id: string;
  /** What a conversation follows: the agent, or the session st found. */
  target: string;
  name: string;
  harness: string;
  state: AgentState;
  unmanaged: boolean;
  /** How long since st last saw it change, e.g. `8h`. */
  activity: string;
  /** The graph path under the name. */
  path: string;
  /** What the step it holds last reported (`st work progress`), on one line: the status read first. */
  progress?: string;
  host: string;
  parent?: string;
};

const labelWords: Record<string, string> = { st3: 'ST', cos: 'COS', ios: 'iOS', tui: 'TUI', pty: 'PTY', omp: 'OMP' };
function label(slug: string): string {
  return slug.split('-').map(word => labelWords[word.toLowerCase()] ?? word.charAt(0).toUpperCase() + word.slice(1)).join(' ');
}

/** An agent's readable name, as stui names it: the last path segment, and `Parent · OMP` for an omp seat. */
export function agentName(agent: Pick<Agent, 'id' | 'name'> & { host_id?: string | null }): string {
  const parts = (agent.name || agent.id).split('/').filter(Boolean);
  // `st agents new NAME` names a seat HOST.NAME; the host shows beside it, so the label is NAME.
  const host = agent.host_id?.replace(/^host\//, '').toLowerCase();
  const dotted = parts.at(-1) ?? agent.id;
  const dot = dotted.indexOf('.');
  const slug = host && dot > 0 && dot < dotted.length - 1 && dotted.slice(0, dot).toLowerCase() === host ? dotted.slice(dot + 1) : dotted;
  if (slug.toLowerCase() === 'omp' && parts.length > 1) return `${label(parts.at(-2)!)} · OMP`;
  return label(slug);
}

// The model its harness last reported using, as reported ("claude-sonnet-5-5"); null when st says none.
export function agentModel(agent: { usage?: { context?: { model?: string | null } | null } | null }): string | null {
  return agent.usage?.context?.model?.trim() || null;
}

export function harnessName(driver: string | null | undefined): string {
  const value = driver ?? '';
  if (value.includes('claude')) return 'claude';
  if (value.includes('codex')) return 'codex';
  if (value.includes('omp')) return 'omp';
  if (value === 'pi') return 'pi';
  return '?';
}

type StateAgent = Pick<Agent, 'state' | 'harness_state' | 'fault' | 'delivery'> & { harness_error_state?: string | null; reason?: string | null; observation?: string | null; reachability?: string | null };
export function agentState(agent: StateAgent): AgentState {
  if (agent.fault) return 'fault';
  // A seat whose message path runs a replaced binary or stopped polling takes no messages,
  // however ready its harness looks.
  if (agent.delivery?.state === 'stale') return 'fault';
  switch (agent.state) {
    case 'failed': return 'fault';
    case 'running': return agent.harness_state === 'working' ? 'working' : 'idle';
    // Signed out of its provider: a login on its host fixes it, without a restart (Nathan, 2026-10-04).
    // st withdraws an idle claim it has not heard renewed lately: the harness reads "indeterminate"
    // and the seat "waiting", though it is up and reachable. That is an idle seat nobody has
    // spoken to, not one starting (Nathan, 2026-10-05).
    case 'waiting': return agent.harness_error_state === 'needs-login' ? 'needs-login' : agent.harness_state === 'indeterminate' && agent.observation === 'stale' && (agent.reachability === 'reachable' || agent.reachability === 'local') ? 'idle' : agent.harness_state === 'unauthenticated' || agent.harness_state === 'needs-login' || agent.reason === 'providerAuth' ? 'needs-login' : agent.harness_state === 'blocked' ? 'needs-you' : 'starting';
    case 'starting':
    case 'desired': return 'starting';
    case 'stopped': return 'stopped';
    default: return 'unknown';
  }
}

export const SPINNER = '⠿';
export function agentGlyph(state: AgentState, spinner = SPINNER): { glyph: string; color: string } {
  switch (state) {
    case 'needs-you': return { glyph: '◆', color: theme.person };
    case 'needs-login': return { glyph: '⚿', color: theme.person };
    case 'fault': return { glyph: '✕', color: theme.fault };
    case 'working': return { glyph: spinner, color: theme.working };
    case 'idle': return { glyph: '●', color: theme.idle };
    case 'starting': return { glyph: '◌', color: theme.waiting };
    case 'stopped': return { glyph: '○', color: theme.quiet };
    case 'unknown': return { glyph: '?', color: theme.quiet };
  }
}

export function agentWord(state: AgentState): string {
  switch (state) {
    case 'needs-you': return 'needs you';
    case 'needs-login': return 'needs login';
    case 'fault': return 'broken';
    case 'working': return 'working';
    case 'idle': return 'idle';
    case 'starting': return 'starting';
    case 'stopped': return 'stopped';
    case 'unknown': return 'not managed';
  }
}

export const UNMANAGED_GROUP = 'found running · not started by st';
export function agentGroup(row: Pick<AgentRowView, 'state' | 'unmanaged'>): string {
  if (row.unmanaged) return UNMANAGED_GROUP;
  switch (row.state) {
    case 'needs-you': return 'waiting on you';
    case 'needs-login': return 'needs login';
    case 'fault': return 'broken';
    case 'working': return 'working';
    case 'idle':
    case 'starting': return 'idle';
    case 'stopped':
    case 'unknown': return 'stopped';
  }
}

export const AGENT_LEGEND: ReadonlyArray<{ state: AgentState; word: string }> = [
  { state: 'needs-you', word: 'needs you' },
  { state: 'needs-login', word: 'needs login' },
  { state: 'working', word: 'working' },
  { state: 'idle', word: 'idle' },
  { state: 'fault', word: 'broken' },
  { state: 'stopped', word: 'stopped' },
  { state: 'unknown', word: 'unmanaged' },
];

/** What a step last reported, on one line; the step run's id names it in whichever run holds it. */
export function stepProgress(missions: Mission[], stepId: string): string | undefined {
  for (const mission of missions) {
    for (const run of mission.run_details ?? []) {
      const step = (run.steps ?? []).find(candidate => candidate.id === stepId);
      const line = step?.last_progress?.split(/\s+/).filter(Boolean).join(' ');
      if (step) return line || undefined;
    }
  }
  return undefined;
}

function host(id: string | null | undefined): string {
  return id ? id.replace(/^host\//, '') : '?';
}

/**
 * Every seat st declares, then every running session st found but did not start. Undeclared
 * sessions are named `driver in workspace` and belong to the gateway's host.
 */
export function agentRows(agents: Agent[], sessions: SessionView[], gatewayHost: string, now = Date.now(), missions: Mission[] = []): AgentRowView[] {
  const declared = agents.map((agent): AgentRowView => ({
    progress: agent.current_work?.[0] ? stepProgress(missions, agent.current_work[0].id) : undefined,
    id: agent.id,
    target: agent.id,
    name: agentName(agent),
    harness: harnessName(agent.driver),
    state: agentState(agent),
    unmanaged: false,
    activity: ago(agent.updated_at, now),
    path: agent.id.replace(/^agent\//, ''),
    host: host(agent.host_id),
    parent: agent.under?.[0]?.agent_id,
  }));
  const found = sessions.filter(session => session.state === 'running' && session.managed === false).map((session): AgentRowView => {
    const workspace = (session as { workspace?: unknown }).workspace;
    const folder = typeof workspace === 'string' ? workspace.split('/').filter(Boolean).pop() ?? workspace : '?';
    return {
      id: session.id,
      target: session.id,
      name: `${session.driver ?? 'harness'} in ${folder}`,
      harness: harnessName(session.driver),
      state: 'unknown',
      unmanaged: true,
      activity: ago(session.updated_at, now),
      path: session.id,
      host: gatewayHost,
    };
  });
  return sortAgentRows([...declared, ...found]);
}

/** stui's order: managed before found, then by state, then by name. */
export function sortAgentRows(rows: AgentRowView[]): AgentRowView[] {
  return [...rows].sort((a, b) =>
    Number(a.unmanaged) - Number(b.unmanaged)
    || AGENT_STATES.indexOf(a.state) - AGENT_STATES.indexOf(b.state)
    || a.name.toLowerCase().localeCompare(b.name.toLowerCase()));
}

/** Rows whose name, path, harness, or host contain every word of the filter, ignoring case. */
export function filterAgentRows(rows: AgentRowView[], filter: string): AgentRowView[] {
  const words = filter.toLowerCase().split(/\s+/).filter(Boolean);
  if (!words.length) return rows;
  return rows.filter(row => {
    const haystack = `${row.name} ${row.path} ${row.harness} ${row.host}`.toLowerCase();
    return words.every(word => haystack.includes(word));
  });
}

export type AgentSection ={ title: string; count: number; person: boolean; rows: AgentRowView[] };
/** Consecutive rows that share a group, each with its header. */
export function agentSections(rows: AgentRowView[]): AgentSection[] {
  const sections: AgentSection[] = [];
  for (const row of rows) {
    const title = agentGroup(row);
    const last = sections.at(-1);
    if (last?.title === title) { last.rows.push(row); last.count++; }
    else sections.push({ title, count: 1, person: (row.state === 'needs-you' || row.state === 'needs-login') && !row.unmanaged, rows: [row] });
  }
  return sections;
}

export type TreeLine = { kind: 'folder'; key: string; depth: number; name: string } | { kind: 'row'; key: string; depth: number; row: AgentRowView };
/**
 * The tree view: each agent under the folders of its graph path, the way stui's tree listing
 * draws it. Folders open once, in path order; a row sits one level below its last folder.
 */
export function agentTreeLines(rows: AgentRowView[]): TreeLine[] {
  const leaves = rows.map(row => ({ path: (row.unmanaged ? `found/${row.host}/${row.name}` : row.path).split('/').filter(Boolean), row }));
  leaves.sort((a, b) => {
    for (let index = 0; index < Math.min(a.path.length, b.path.length); index++) {
      if (a.path[index] !== b.path[index]) return a.path[index] < b.path[index] ? -1 : 1;
    }
    return a.path.length - b.path.length;
  });
  const compact = compactFolders(leaves.map(leaf => leaf.path));
  const lines: TreeLine[] = [];
  let open: string[] = [];
  for (const [index, { row }] of leaves.entries()) {
    const folders = compact[index];
    let shared = 0;
    while (shared < open.length && shared < folders.length && open[shared] === folders[shared]) shared++;
    open = open.slice(0, shared);
    for (let depth = shared; depth < folders.length; depth++) {
      open.push(folders[depth]);
      lines.push({ kind: 'folder', key: `folder:${open.join('/')}`, depth, name: folders[depth] });
    }
    lines.push({ kind: 'row', key: row.id, depth: folders.length, row });
  }
  return lines;
}

/**
 * Each path's folders, with a folder that holds only one folder joined to it on one line
 * (Nathan, 2026-10-01): `fleet/smalltalk/operations` rather than three nested folders.
 */
export function compactFolders(paths: string[][]): string[][] {
  const holds = new Map<string, Set<string>>();
  for (const path of paths) {
    const folders = path.slice(0, -1);
    folders.forEach((_, depth) => {
      const key = folders.slice(0, depth + 1).join('/');
      const children = holds.get(key) ?? new Set<string>();
      children.add(folders[depth + 1] ?? '');
      holds.set(key, children);
    });
  }
  return paths.map(path => {
    const folders = path.slice(0, -1);
    const compact: string[] = [];
    for (let depth = 0; depth < folders.length; depth++) {
      let name = folders[depth];
      while (depth + 1 < folders.length && holds.get(folders.slice(0, depth + 1).join('/'))?.size === 1) {
        depth++;
        name = `${name}/${folders[depth]}`;
      }
      compact.push(name);
    }
    return compact;
  });
}

export { harnessColor };

/** What a person does for an agent signed out of its provider, as stui says it. */
export function loginGuidance(agent: { driver?: string | null; host_id?: string | null }): string {
  const host = agent.host_id?.replace(/^host\//, '') ?? 'its host';
  const harness = harnessName(agent.driver);
  const how = harness === 'claude' ? 'open its terminal and run /login' : harness === 'codex' ? `run \`codex login\` on ${host} (or in its terminal)` : 'open its terminal and log it in';
  const provider = harness === 'claude' ? 'Claude' : harness === 'codex' ? 'Codex' : 'Its provider';
  return `${provider} login required on ${host}: ${how}. Messages wait until it is signed in; it carries on without a restart.`;
}
