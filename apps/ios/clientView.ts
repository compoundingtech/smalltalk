// The view model every Small Talk client draws: the TypeScript twin of stui's `ui::view`.
//
// The live adapter and the demo fixture (fixtures/clients/demo-world.json) both produce a
// `World`. Anything the graph cannot answer yet is optional or a `Load`, so a screen can say
// "unknown" instead of guessing. The JSON shape is exactly stui's serde output.

/** Something that arrives later. `loading` is never drawn as empty. */
export type Load<T> = { state: 'loading' } | { state: 'ready'; value: T } | { state: 'failed'; value: string };

export type Link = 'live' | 'connecting' | { offline: string };

export type World = {
  person: string;
  host: string;
  link: Link;
  attention: Load<Attention[]>;
  agents: Load<Agent[]>;
  missions: Load<Mission[]>;
  machines: Load<Machine[]>;
  worktrees: Load<Worktree[]>;
  /** The person's paired devices. */
  devices: Load<Device[]>;
  conversations: Record<string, Load<Entry[]>>;
  /** Missions nobody needs the person for, counted so Home can say what it is not showing. */
  quiet_missions: number;
};

export type Tier = 'stopped' | 'alert' | 'today' | 'later';

export type Attention = {
  id: string;
  tier: Tier;
  title: string;
  /** Who is waiting, e.g. "Atlas Builder on harbor". */
  waiting: string | null;
  age: string;
  mission: string | null;
  /** The agent that did the work or raised the item: who to talk to about it. */
  agent: string | null;
  kind: AttentionKind;
  /** The actions st offers for this item. Empty in the demo, where every button works. */
  actions: string[];
  /** Graph subjects the item points at, with their state when st says. */
  related: [string, string | null][];
  /** Who raised it, when st says. */
  raised_by: string | null;
};

export type AttentionKind =
  | { kind: 'review'; question: string; because: string; look_at: [string, string][]; step: string }
  | { kind: 'feedback'; question: string; subject: string; excerpt: string[]; link: string | null }
  | { kind: 'launch'; planner: string; name: string; preview: Load<MissionPreview> }
  | { kind: 'revision'; reason: string; changes: [string, string][] }
  | { kind: 'fault'; what: string; because: string; fix: string | null; source: string }
  | { kind: 'message'; from: string; body: string };

export type AttentionKindWord = AttentionKind['kind'];

export type MissionPreview = {
  name: string;
  goals: string[];
  steps: PreviewStep[];
  agents: PreviewAgent[];
  workspace: string;
};
export type PreviewStep = { name: string; assignee: string; after: string[]; asks_you: boolean };
export type PreviewAgent = { name: string; harness: Harness; host: string };

export type Harness = 'claude' | 'codex' | 'omp' | 'pi' | 'unknown';
export const harnessName = (harness: Harness) => (harness === 'unknown' ? '?' : harness);

export type AgentState = 'needs_you' | 'fault' | 'working' | 'idle' | 'starting' | 'stopped' | 'unknown';

export type Agent = {
  /** The graph path, a stable id. */
  id: string;
  name: string;
  harness: Harness;
  state: AgentState;
  host: string;
  worktree: string | null;
  mission: string | null;
  step: string | null;
  activity: string;
  /** Found running on a host but not started by st. */
  unmanaged: boolean;
  parent: string | null;
  details: AgentDetails;
  /** A live terminal the person can open. */
  terminal: boolean;
};

/** What the details section shows about an agent. Every field is optional: st may not say. */
export type AgentDetails = {
  goal: string | null;
  claimed: string | null;
  next: string | null;
  queue: string[];
  queued: number;
  harness_state: string | null;
  runtime: string | null;
  fault: string | null;
  under: string | null;
};

export const emptyDetails = (): AgentDetails => ({ goal: null, claimed: null, next: null, queue: [], queued: 0, harness_state: null, runtime: null, fault: null, under: null });

/** One word naming who has to move, shared by every mission surface. */
export type Word = 'decision' | 'stalled' | 'unstaffed' | 'unclaimed' | 'queued' | 'working' | 'watching' | 'held' | 'idle' | 'done' | 'failed';

export type StepState = 'done' | 'working' | 'ready' | 'waiting' | 'needs_you' | 'failed' | 'pending';

export type Step = {
  name: string;
  state: StepState;
  owner: string | null;
  note: string | null;
  after: string[];
  age: string;
  goals: string[];
  constraints: string[];
  gates: string[];
  attempt: number;
  blockers: string[];
};

export type Mission = {
  id: string;
  title: string;
  word: Word;
  age: string;
  host: string;
  goals: string[];
  steps: Step[];
  agents: string[];
  decision: string | null;
  worktree: string | null;
  parent: string | null;
  system: boolean;
  /** The mission's declaration as written, when st provides it. */
  kdl: string | null;
};

export function progress(mission: Mission): [number, number] {
  return [mission.steps.filter(step => step.state === 'done').length, mission.steps.length];
}

export type Machine = {
  name: string;
  online: boolean;
  platform: string;
  seen: string;
  load: string | null;
  links: [string, boolean, string][];
  you_are_here: boolean;
};

export type Worktree = {
  path: string;
  host: string;
  branch: string;
  ahead: number;
  behind: number;
  dirty: number;
  agents: string[];
  missions: string[];
};

export type Device = { id: string; name: string; state: string; scopes: string[]; expires: string };

export type Entry = { id: string; at: string; body: Body };
export type ToolState = 'running' | 'ok' | 'failed';
export type Body =
  /** Text typed into the harness: the person, or a delivery the harness shows as a prompt. */
  | { kind: 'user'; value: string }
  | { kind: 'assistant'; value: string }
  | { kind: 'thinking'; value: string }
  | { kind: 'tool'; value: { title: string; state: ToolState; output: string[] } }
  /** A Small Talk message between agents or people, shown in the same stream. */
  | { kind: 'mail'; value: { from: string; to: string; subject: string; body: string } }
  /** A graph event worth a line: a step became ready, a run started. */
  | { kind: 'event'; value: string }
  /** A message sent from here that st has not reported back yet, or that failed. */
  | { kind: 'pending'; value: { text: string; failed: string | null } };

export const loading = <T>(): Load<T> => ({ state: 'loading' });
export const ready = <T>(value: T): Load<T> => ({ state: 'ready', value });
export const failed = <T>(value: string): Load<T> => ({ state: 'failed', value });
export function items<T>(load: Load<T[]>): T[] { return load.state === 'ready' ? load.value : []; }

// ------------------------------------------------------------------ decoding

// The demo fixture is data from outside the app bundle's type system; read it field by field so
// a drift between stui and this app fails loudly instead of drawing half a screen.

class Shape extends Error {}
function fail(path: string, expected: string): never { throw new Shape(`${path}: expected ${expected}`); }
type Reader<T> = (value: unknown, path: string) => T;
const str: Reader<string> = (value, path) => (typeof value === 'string' ? value : fail(path, 'a string'));
const num: Reader<number> = (value, path) => (typeof value === 'number' && Number.isFinite(value) ? value : fail(path, 'a number'));
const bool: Reader<boolean> = (value, path) => (typeof value === 'boolean' ? value : fail(path, 'a boolean'));
const nullable = <T>(read: Reader<T>): Reader<T | null> => (value, path) => (value === null ? null : read(value, path));
const list = <T>(read: Reader<T>): Reader<T[]> => (value, path) => (Array.isArray(value) ? value.map((item, index) => read(item, `${path}[${index}]`)) : fail(path, 'a list'));
function oneOf<T extends string>(...options: T[]): Reader<T> {
  return (value, path) => (options.includes(value as T) ? value as T : fail(path, options.join(' | ')));
}
function tuple<T extends unknown[]>(...reads: { [K in keyof T]: Reader<T[K]> }): Reader<T> {
  return (value, path) => {
    if (!Array.isArray(value) || value.length !== reads.length) fail(path, `a ${reads.length}-tuple`);
    return reads.map((read, index) => read(value[index], `${path}[${index}]`)) as T;
  };
}
function record(value: unknown, path: string): Record<string, unknown> {
  return value && typeof value === 'object' && !Array.isArray(value) ? value as Record<string, unknown> : fail(path, 'an object');
}
function struct<T>(fields: { [K in keyof T]: Reader<T[K]> }): Reader<T> {
  return (value, path) => {
    const source = record(value, path);
    for (const key of Object.keys(source)) if (!(key in fields)) fail(`${path}.${key}`, 'no such field');
    const out = {} as T;
    for (const key of Object.keys(fields) as (keyof T & string)[]) out[key] = fields[key](source[key], `${path}.${key}`);
    return out;
  };
}
function load<T>(read: Reader<T>): Reader<Load<T>> {
  return (value, path) => {
    const source = record(value, path);
    switch (source.state) {
      case 'loading': return { state: 'loading' };
      case 'ready': return { state: 'ready', value: read(source.value, `${path}.value`) };
      case 'failed': return { state: 'failed', value: str(source.value, `${path}.value`) };
      default: return fail(`${path}.state`, 'loading | ready | failed');
    }
  };
}

const harness = oneOf<Harness>('claude', 'codex', 'omp', 'pi', 'unknown');
const previewRead: Reader<MissionPreview> = struct<MissionPreview>({
  name: str,
  goals: list(str),
  steps: list(struct<PreviewStep>({ name: str, assignee: str, after: list(str), asks_you: bool })),
  agents: list(struct<PreviewAgent>({ name: str, harness, host: str })),
  workspace: str,
});
const kindRead: Reader<AttentionKind> = (value, path) => {
  const source = record(value, path);
  switch (source.kind) {
    case 'review': return struct<Extract<AttentionKind, { kind: 'review' }>>({ kind: oneOf('review'), question: str, because: str, look_at: list(tuple<[string, string]>(str, str)), step: str })(value, path);
    case 'feedback': return struct<Extract<AttentionKind, { kind: 'feedback' }>>({ kind: oneOf('feedback'), question: str, subject: str, excerpt: list(str), link: nullable(str) })(value, path);
    case 'launch': return struct<Extract<AttentionKind, { kind: 'launch' }>>({ kind: oneOf('launch'), planner: str, name: str, preview: load(previewRead) })(value, path);
    case 'revision': return struct<Extract<AttentionKind, { kind: 'revision' }>>({ kind: oneOf('revision'), reason: str, changes: list(tuple<[string, string]>(str, str)) })(value, path);
    case 'fault': return struct<Extract<AttentionKind, { kind: 'fault' }>>({ kind: oneOf('fault'), what: str, because: str, fix: nullable(str), source: str })(value, path);
    case 'message': return struct<Extract<AttentionKind, { kind: 'message' }>>({ kind: oneOf('message'), from: str, body: str })(value, path);
    default: return fail(`${path}.kind`, 'an attention kind');
  }
};
const attentionRead = struct<Attention>({
  id: str, tier: oneOf<Tier>('stopped', 'alert', 'today', 'later'), title: str, waiting: nullable(str), age: str,
  mission: nullable(str), agent: nullable(str), kind: kindRead, actions: list(str),
  related: list(tuple<[string, string | null]>(str, nullable(str))), raised_by: nullable(str),
});
const agentRead = struct<Agent>({
  id: str, name: str, harness, state: oneOf<AgentState>('needs_you', 'fault', 'working', 'idle', 'starting', 'stopped', 'unknown'),
  host: str, worktree: nullable(str), mission: nullable(str), step: nullable(str), activity: str, unmanaged: bool, parent: nullable(str),
  details: struct<AgentDetails>({ goal: nullable(str), claimed: nullable(str), next: nullable(str), queue: list(str), queued: num, harness_state: nullable(str), runtime: nullable(str), fault: nullable(str), under: nullable(str) }),
  terminal: bool,
});
const stepRead = struct<Step>({
  name: str, state: oneOf<StepState>('done', 'working', 'ready', 'waiting', 'needs_you', 'failed', 'pending'), owner: nullable(str), note: nullable(str),
  after: list(str), age: str, goals: list(str), constraints: list(str), gates: list(str), attempt: num, blockers: list(str),
});
const missionRead = struct<Mission>({
  id: str, title: str, word: oneOf<Word>('decision', 'stalled', 'unstaffed', 'unclaimed', 'queued', 'working', 'watching', 'held', 'idle', 'done', 'failed'),
  age: str, host: str, goals: list(str), steps: list(stepRead), agents: list(str), decision: nullable(str), worktree: nullable(str), parent: nullable(str), system: bool, kdl: nullable(str),
});
const machineRead = struct<Machine>({ name: str, online: bool, platform: str, seen: str, load: nullable(str), links: list(tuple<[string, boolean, string]>(str, bool, str)), you_are_here: bool });
const deviceRead = struct<Device>({ id: str, name: str, state: str, scopes: list(str), expires: str });
const worktreeRead = struct<Worktree>({ path: str, host: str, branch: str, ahead: num, behind: num, dirty: num, agents: list(str), missions: list(str) });

export const readBody: Reader<Body> = (value, path) => {
  const source = record(value, path);
  const inner = `${path}.value`;
  switch (source.kind) {
    case 'user': case 'assistant': case 'thinking': case 'event':
      return struct<{ kind: 'user'; value: string }>({ kind: oneOf('user', 'assistant', 'thinking', 'event') as Reader<'user'>, value: str })(value, path);
    case 'tool': return { kind: 'tool', value: struct<{ title: string; state: ToolState; output: string[] }>({ title: str, state: oneOf<ToolState>('running', 'ok', 'failed'), output: list(str) })(source.value, inner) };
    case 'mail': return { kind: 'mail', value: struct<{ from: string; to: string; subject: string; body: string }>({ from: str, to: str, subject: str, body: str })(source.value, inner) };
    case 'pending': return { kind: 'pending', value: struct<{ text: string; failed: string | null }>({ text: str, failed: nullable(str) })(source.value, inner) };
    default: return fail(`${path}.kind`, 'a body kind');
  }
};
const entryRead = struct<Entry>({ id: str, at: str, body: readBody });

const linkRead: Reader<Link> = (value, path) => {
  if (value === 'live' || value === 'connecting') return value;
  return { offline: str(record(value, path).offline, `${path}.offline`) };
};

/** Read a serialized `World` (the demo fixture), failing on any field stui did not write. */
export function decodeWorld(value: unknown): World {
  const conversations: Record<string, Load<Entry[]>> = {};
  const source = record(value, 'world');
  for (const [key, conversation] of Object.entries(record(source.conversations, 'world.conversations'))) {
    conversations[key] = load(list(entryRead))(conversation, `world.conversations.${key}`);
  }
  return struct<World>({
    person: str, host: str, link: linkRead,
    attention: load(list(attentionRead)), agents: load(list(agentRead)), missions: load(list(missionRead)),
    machines: load(list(machineRead)), worktrees: load(list(worktreeRead)), devices: load(list(deviceRead)),
    conversations: () => conversations, quiet_missions: num,
  })(value, 'world');
}
