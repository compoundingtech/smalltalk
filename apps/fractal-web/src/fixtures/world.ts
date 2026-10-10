/**
 * The one deterministic fixture world every story renders.
 *
 * This module is the identity graph: hosts, agents and their sessions, repositories, missions,
 * pull requests, CI runs, quota accounts, Vista publications and the open decision escalation.
 * Feature fixtures (`src/<feature>/fixtures.ts`) project these entities into their wire or view
 * models; they never invent agents, missions, refs or clocks of their own.
 *
 * Invariants (checked by `assertWorld` at module load):
 * - every time derives from `worldNow` (2026-10-01T15:30:00Z) through `minutesAgo`;
 * - every cross-reference (agent → host/mission/repo, PR → mission/agent/repo, CI → PR) resolves;
 * - the data is public-safe: neutral hosts (`build-host-*`), a fictional `acme` org, `/srv/work`
 *   paths, no credentials or secret references.
 */

// ── Clock ─────────────────────────────────────────────────────────────────────────────────────

/** The single pinned "now" of the fixture world. */
export const WORLD_NOW_ISO = '2026-10-01T15:30:00Z'
/** Epoch milliseconds of the pinned fixture clock. */
export const worldNow = Date.parse(WORLD_NOW_ISO)

const iso = (ms: number) => new Date(ms).toISOString().replace('.000Z', 'Z')

/** ISO timestamp `minutes` before `worldNow` (fractions allowed for seconds). */
export const minutesAgo = (minutes: number): string => iso(worldNow - Math.round(minutes * 60_000))
/** ISO timestamp `hours` before `worldNow`. */
export const hoursAgo = (hours: number): string => minutesAgo(hours * 60)
/** ISO timestamp `days` before `worldNow`. */
export const daysAgo = (days: number): string => minutesAgo(days * 1440)
/** `HH:MM:SS` (UTC) of a timestamp `minutes` before `worldNow`, for log-style lines. */
export const clockAgo = (minutes: number): string => minutesAgo(minutes).slice(11, 19)

// ── Hosts ─────────────────────────────────────────────────────────────────────────────────────

/** Public-safe identifiers for hosts in the fixture fleet. */
export type HostId = 'build-host-a' | 'build-host-b' | 'build-host-c'

/** Host connectivity and identity projected into fleet views. */
export interface WorldHost {
  readonly id: HostId
  /** `host/<id>` */
  readonly ref: string
  readonly role: string
  readonly os: string
  readonly connected: boolean
  /** Last heartbeat seen by the gateway. */
  readonly lastSeen: string
}

/** Deterministic fleet with two connected builders and one offline laptop. */
export const hosts: readonly WorldHost[] = [
  {
    id: 'build-host-a',
    ref: 'host/build-host-a',
    role: 'gateway + interactive agents',
    os: 'NixOS 25.05 · x86_64',
    connected: true,
    lastSeen: minutesAgo(0.1),
  },
  {
    id: 'build-host-b',
    ref: 'host/build-host-b',
    role: 'scheduled missions + CI helpers',
    os: 'NixOS 25.05 · x86_64',
    connected: true,
    lastSeen: minutesAgo(0.2),
  },
  {
    id: 'build-host-c',
    ref: 'host/build-host-c',
    role: 'laptop (Darwin builder)',
    os: 'macOS 26.0 · arm64',
    connected: false,
    lastSeen: minutesAgo(47),
  },
]

/** The host running the st gateway the workbench is connected to. */
export const gatewayHost: HostId = 'build-host-a'

// ── People ────────────────────────────────────────────────────────────────────────────────────

/** Public-safe person identity used for ownership and reviews. */
export interface WorldPerson {
  readonly ref: string
  readonly name: string
  readonly handle: string
}

/** The human operator who owns the fleet and answers escalations. */
export const operator: WorldPerson = { ref: 'person/operator', name: 'Sam Okafor', handle: 'sokafor' }
/** A second reviewer on pull requests. */
export const reviewer: WorldPerson = {
  ref: 'person/reviewer',
  name: 'Priya Natarajan',
  handle: 'pnatarajan',
}
/** The GitHub account agents push and open pull requests as. */
export const botAccount = 'acme-agents[bot]'

// ── Repositories ──────────────────────────────────────────────────────────────────────────────

/** Fictional repositories shared by every feature fixture. */
export type RepoId = 'acme/webfractal' | 'acme/gateway' | 'acme/platform' | 'acme/docs'

/** Repository identity and metadata for resource navigation. */
export interface WorldRepo {
  readonly id: RepoId
  /** `resource/github/<owner>/<name>` */
  readonly ref: string
  readonly url: string
  readonly description: string
  readonly defaultBranch: 'main'
}

const repo = ({ id, description }: { id: RepoId; description: string }): WorldRepo => ({
  id,
  ref: `resource/github/${id}`,
  url: `https://github.com/${id}`,
  description,
  defaultBranch: 'main',
})

/** Repository catalog referenced by agents, missions and pull requests. */
export const repos: readonly WorldRepo[] = [
  repo({
    id: 'acme/webfractal',
    description: 'Web workbench client for the agent fleet (React, StyleX, React Aria)',
  }),
  repo({
    id: 'acme/gateway',
    description: 'st gateway: RPC + collection sockets over the fleet state',
  }),
  repo({
    id: 'acme/platform',
    description: 'Nix flake for hosts, builders, CI runners and scheduled missions',
  }),
  repo({ id: 'acme/docs', description: 'Runbooks and architecture notes' }),
]

/** Checkout path of a repository on any host. */
export const workdir = ({ repoId, branch }: { repoId: RepoId; branch: string }): string =>
  branch === 'main'
    ? `/srv/work/${repoId.split('/')[1]}`
    : `/srv/work/${repoId.split('/')[1]}--${branch.replaceAll('/', '-')}`

// ── Agents and sessions ───────────────────────────────────────────────────────────────────────

/** Harness families represented by fixture agent sessions. */
export type Harness = 'omp' | 'claude' | 'codex'
/** Current activity displayed for a fixture agent. */
export type AgentActivity = 'working' | 'waiting' | 'idle' | 'errored'

/** Harness session metadata, usage and checkout for one agent. */
export interface WorldSession {
  /** `session/<ulid>` */
  readonly id: string
  readonly harness: Harness
  /** Harness display, e.g. `Claude Code 2.4.1`. */
  readonly client: string
  readonly model: string
  readonly cwd: string
  readonly branch: string
  readonly startedAt: string
  readonly lastActivityAt: string
  readonly turns: number
  readonly toolCalls: number
  readonly tokens: { readonly input: number; readonly output: number; readonly cached: number }
  /** Context window fill, 0..1. */
  readonly contextFill: number
}

/** Host-qualified agent identity and its current session. */
export interface WorldAgent {
  readonly slug: string
  /** `agent/<host>/<slug>` */
  readonly ref: string
  /** `terminal/<host>/<slug>`: the PTY the agent's harness runs in. */
  readonly terminal: string
  /** PascalCase display name, as agents name themselves in the fleet. */
  readonly name: string
  readonly host: HostId
  readonly activity: AgentActivity
  /** One-line status, as the agents navigator shows it. */
  readonly status: string
  readonly repo: RepoId
  /** `mission/<path>` the agent works for, when any. */
  readonly mission?: string
  readonly session: WorldSession
}

const clients: Readonly<Record<Harness, { readonly client: string; readonly model: string }>> = {
  omp: { client: 'omp 0.42.0', model: 'claude-opus-4-5' },
  claude: { client: 'Claude Code 2.4.1', model: 'claude-sonnet-4-5' },
  codex: { client: 'Codex CLI 0.61.0', model: 'gpt-5-codex' },
}

interface AgentSpec {
  readonly host: HostId
  readonly slug: string
  readonly name: string
  readonly harness: Harness
  readonly activity: AgentActivity
  readonly status: string
  readonly repo: RepoId
  readonly mission?: string
  readonly branch: string
  /** Session start / last activity, in minutes before `worldNow`. */
  readonly started: number
  readonly lastActive: number
  readonly turns: number
  readonly toolCalls: number
  readonly contextFill: number
  /** Stable 10-char suffix for the session id. */
  readonly sid: string
}

const agent = (spec: AgentSpec): WorldAgent => {
  const { client, model } = clients[spec.harness]
  // Token counts follow turns and context fill so busier sessions look busier.
  const input = spec.turns * 18_400 + Math.round(spec.contextFill * 120_000)
  return {
    slug: spec.slug,
    ref: `agent/${spec.host}/${spec.slug}`,
    terminal: `terminal/${spec.host}/${spec.slug}`,
    name: spec.name,
    host: spec.host,
    activity: spec.activity,
    status: spec.status,
    repo: spec.repo,
    ...(spec.mission === undefined ? {} : { mission: spec.mission }),
    session: {
      id: `session/01JDQ${spec.sid}`,
      harness: spec.harness,
      client,
      model,
      cwd: workdir({ repoId: spec.repo, branch: spec.branch }),
      branch: spec.branch,
      startedAt: minutesAgo(spec.started),
      lastActivityAt: minutesAgo(spec.lastActive),
      turns: spec.turns,
      toolCalls: spec.toolCalls,
      tokens: { input, output: Math.round(input * 0.071), cached: Math.round(input * 0.83) },
      contextFill: spec.contextFill,
    },
  }
}

/** Fleet agents spanning active, waiting, idle and errored sessions. */
export const agents: readonly WorldAgent[] = [
  // build-host-a: gateway host, interactive webfractal + gateway work
  agent({
    host: 'build-host-a',
    slug: 'workbench-shell',
    name: 'WorkbenchShell',
    harness: 'omp',
    activity: 'working',
    status: 'running storybook build',
    repo: 'acme/webfractal',
    mission: 'mission/webfractal/foundation',
    branch: 'agent/workbench-shell',
    started: 212,
    lastActive: 0.4,
    turns: 41,
    toolCalls: 318,
    contextFill: 0.62,
    sid: '6ZK4WS0A1',
  }),
  agent({
    host: 'build-host-a',
    slug: 'terminal-renderer',
    name: 'TerminalRenderer',
    harness: 'omp',
    activity: 'working',
    status: 'benchmarking ghostty-web frames',
    repo: 'acme/webfractal',
    mission: 'mission/webfractal/foundation',
    branch: 'agent/terminal-renderer',
    started: 188,
    lastActive: 1.2,
    turns: 27,
    toolCalls: 204,
    contextFill: 0.48,
    sid: '6ZM1TR0B2',
  }),
  agent({
    host: 'build-host-a',
    slug: 'missions-ui',
    name: 'MissionsUi',
    harness: 'omp',
    activity: 'waiting',
    status: 'waiting on decision D12',
    repo: 'acme/webfractal',
    mission: 'mission/webfractal/foundation',
    branch: 'agent/missions-ui',
    started: 175,
    lastActive: 9,
    turns: 33,
    toolCalls: 241,
    contextFill: 0.71,
    sid: '6ZP8MU0C3',
  }),
  agent({
    host: 'build-host-a',
    slug: 'palette-review',
    name: 'PaletteReview',
    harness: 'claude',
    activity: 'idle',
    status: 'review posted on #221',
    repo: 'acme/webfractal',
    mission: 'mission/webfractal/command-palette',
    branch: 'agent/palette-review',
    started: 96,
    lastActive: 22,
    turns: 12,
    toolCalls: 58,
    contextFill: 0.29,
    sid: '6ZR3PR0D4',
  }),
  agent({
    host: 'build-host-a',
    slug: 'gateway-schema',
    name: 'GatewaySchema',
    harness: 'codex',
    activity: 'working',
    status: 'regenerating client models',
    repo: 'acme/gateway',
    mission: 'mission/gateway/schema-v1',
    branch: 'agent/gateway-schema',
    started: 141,
    lastActive: 0.8,
    turns: 19,
    toolCalls: 133,
    contextFill: 0.44,
    sid: '6ZT6GS0E5',
  }),
  // build-host-b: scheduled platform missions
  agent({
    host: 'build-host-b',
    slug: 'deps-steward',
    name: 'DepsSteward',
    harness: 'omp',
    activity: 'working',
    status: 'proving bumped flakes',
    repo: 'acme/platform',
    mission: 'mission/platform/deps/weekly-cycle',
    branch: 'agent/deps-steward-w40',
    started: 384,
    lastActive: 2.5,
    turns: 58,
    toolCalls: 402,
    contextFill: 0.81,
    sid: '6YB2DS0F6',
  }),
  agent({
    host: 'build-host-b',
    slug: 'health-probe',
    name: 'HealthProbe',
    harness: 'omp',
    activity: 'idle',
    status: 'daily run 274 completed',
    repo: 'acme/platform',
    mission: 'mission/platform/health/daily',
    branch: 'main',
    started: 455,
    lastActive: 318,
    turns: 9,
    toolCalls: 71,
    contextFill: 0.18,
    sid: '6Y7HP0G7A',
  }),
  agent({
    host: 'build-host-b',
    slug: 'cas-janitor',
    name: 'CasJanitor',
    harness: 'codex',
    activity: 'errored',
    status: 'ENOSPC on /srv/cas-staging',
    repo: 'acme/platform',
    mission: 'mission/platform/storage/cas-migration',
    branch: 'agent/cas-janitor',
    started: 266,
    lastActive: 4,
    turns: 23,
    toolCalls: 187,
    contextFill: 0.53,
    sid: '6YD9CJ0H8',
  }),
  agent({
    host: 'build-host-b',
    slug: 'ci-doctor',
    name: 'CiDoctor',
    harness: 'claude',
    activity: 'working',
    status: 'bisecting flaky check',
    repo: 'acme/platform',
    mission: 'mission/platform/ci/flake-triage',
    branch: 'agent/ci-doctor',
    started: 74,
    lastActive: 0.2,
    turns: 14,
    toolCalls: 96,
    contextFill: 0.37,
    sid: '6ZV0CD0J9',
  }),
  agent({
    host: 'build-host-b',
    slug: 'docs-writer',
    name: 'DocsWriter',
    harness: 'claude',
    activity: 'waiting',
    status: 'review requested on docs#57',
    repo: 'acme/docs',
    mission: 'mission/docs/runbooks-refresh',
    branch: 'agent/runbooks-refresh',
    started: 133,
    lastActive: 15,
    turns: 21,
    toolCalls: 88,
    contextFill: 0.41,
    sid: '6ZW4DW0KA',
  }),
  // build-host-c: laptop, currently offline
  agent({
    host: 'build-host-c',
    slug: 'sdk-interop',
    name: 'SdkInterop',
    harness: 'claude',
    activity: 'working',
    status: 'last seen 47m ago',
    repo: 'acme/gateway',
    mission: 'mission/gateway/sdk-interop',
    branch: 'agent/sdk-interop',
    started: 290,
    lastActive: 47,
    turns: 31,
    toolCalls: 176,
    contextFill: 0.66,
    sid: '6YE5SI0MB',
  }),
  agent({
    host: 'build-host-c',
    slug: 'web-scout',
    name: 'WebScout',
    harness: 'omp',
    activity: 'idle',
    status: 'scouted virtualization libs',
    repo: 'acme/webfractal',
    branch: 'main',
    started: 520,
    lastActive: 402,
    turns: 7,
    toolCalls: 44,
    contextFill: 0.22,
    sid: '6XZ1WS0NC',
  }),
]

// ── Missions ──────────────────────────────────────────────────────────────────────────────────

/** Lifecycle states represented by fixture missions. */
export type MissionState =
  | 'draft'
  | 'proposed'
  | 'active'
  | 'running'
  | 'paused'
  | 'completed'
  | 'failed'
  | 'cancelled'
/** Who must act next to advance a fixture mission. */
export type MustAct = 'none' | 'agent' | 'human'

/** Mission identity, lifecycle and assigned agent references. */
export interface WorldMission {
  /** Mission path, e.g. `webfractal/foundation`. */
  readonly path: string
  /** `mission/<path>` */
  readonly ref: string
  readonly title: string
  readonly goal: string
  readonly state: MissionState
  readonly mustAct: MustAct
  readonly repo: RepoId
  /** Agent refs working this mission (subset of `agents`). */
  readonly agents: readonly string[]
  /** Recurring schedule, when the mission is a cycle. */
  readonly schedule?: string
  /** Total runs so far; the latest is `runs`. */
  readonly runs: number
  readonly updatedAt: string
}

interface MissionSpec extends Omit<WorldMission, 'ref' | 'agents' | 'updatedAt'> {
  readonly updated: number
}

const missionOf = (spec: MissionSpec): WorldMission => {
  const ref = `mission/${spec.path}`
  const { updated, ...rest } = spec
  return {
    ...rest,
    ref,
    agents: agents.filter((a) => a.mission === ref).map((a) => a.ref),
    updatedAt: minutesAgo(updated),
  }
}

/** Mission catalog covering interactive work and scheduled cycles. */
export const missions: readonly WorldMission[] = [
  missionOf({
    path: 'webfractal/foundation',
    title: 'webfractal: workbench foundation',
    goal: 'Ship the IDE workbench shell with sessions, missions and resources wired to the st gateway.',
    state: 'running',
    mustAct: 'human',
    repo: 'acme/webfractal',
    runs: 3,
    updated: 9,
  }),
  missionOf({
    path: 'webfractal/command-palette',
    title: 'webfractal: command palette',
    goal: 'Unified palette over commands, subjects and subject actions with fenced dispatch.',
    state: 'running',
    mustAct: 'agent',
    repo: 'acme/webfractal',
    runs: 2,
    updated: 22,
  }),
  missionOf({
    path: 'webfractal/terminal-renderer',
    title: 'webfractal: terminal renderer bake-off',
    goal: 'Pick a terminal renderer by frame cost and fidelity on recorded agent sessions.',
    state: 'completed',
    mustAct: 'none',
    repo: 'acme/webfractal',
    runs: 1,
    updated: 1440,
  }),
  missionOf({
    path: 'webfractal/vista-embed',
    title: 'webfractal: embed Vista apps natively',
    goal: 'Render published Vista apps in-process with an explicit native-code trust policy.',
    state: 'completed',
    mustAct: 'none',
    repo: 'acme/webfractal',
    runs: 2,
    updated: 2880,
  }),
  missionOf({
    path: 'gateway/schema-v1',
    title: 'gateway: freeze client schema v1',
    goal: 'Freeze st3.client v1 envelopes and regenerate the TypeScript and Rust clients.',
    state: 'running',
    mustAct: 'agent',
    repo: 'acme/gateway',
    runs: 1,
    updated: 1,
  }),
  missionOf({
    path: 'gateway/sdk-interop',
    title: 'gateway: SDK interop matrix',
    goal: 'Prove the TypeScript SDK against the Rust gateway on Linux and Darwin.',
    state: 'paused',
    mustAct: 'human',
    repo: 'acme/gateway',
    runs: 2,
    updated: 47,
  }),
  missionOf({
    path: 'gateway/rate-limits',
    title: 'gateway: per-client follow budgets',
    goal: 'Bound live follows per client so one tab cannot starve the collection socket.',
    state: 'proposed',
    mustAct: 'human',
    repo: 'acme/gateway',
    runs: 0,
    updated: 180,
  }),
  missionOf({
    path: 'platform/deps/weekly-cycle',
    title: 'platform: weekly dependency cycle',
    goal: 'Keep external flakes current through verified, judgment-first pull requests.',
    state: 'running',
    mustAct: 'agent',
    repo: 'acme/platform',
    schedule: 'weekly · Mon 07:00 UTC',
    runs: 40,
    updated: 2.5,
  }),
  missionOf({
    path: 'platform/health/daily',
    title: 'platform: daily fleet health',
    goal: 'Probe every host daily and open a finding for each failed invariant.',
    state: 'active',
    mustAct: 'human',
    repo: 'acme/platform',
    schedule: 'daily · 06:00 UTC',
    runs: 274,
    updated: 318,
  }),
  missionOf({
    path: 'platform/storage/cas-migration',
    title: 'platform: migrate CAS to content-addressed store',
    goal: 'Move build artefacts into the CAS, verify every blob, then delete the originals.',
    state: 'running',
    mustAct: 'human',
    repo: 'acme/platform',
    runs: 1,
    updated: 4,
  }),
  missionOf({
    path: 'platform/ci/flake-triage',
    title: 'platform: triage flaky CI checks',
    goal: 'Find and quarantine checks that fail without a code change.',
    state: 'running',
    mustAct: 'agent',
    repo: 'acme/platform',
    runs: 1,
    updated: 0.2,
  }),
  missionOf({
    path: 'platform/secrets/rotation',
    title: 'platform: rotate CI deploy keys',
    goal: 'Rotate every CI deploy key and prove no runner still uses the old ones.',
    state: 'completed',
    mustAct: 'none',
    repo: 'acme/platform',
    runs: 1,
    updated: 4320,
  }),
  missionOf({
    path: 'platform/observability/otel-cutover',
    title: 'platform: OTLP cutover',
    goal: 'Move agent telemetry to the shared OTLP collector and retire the sidecar exporters.',
    state: 'draft',
    mustAct: 'human',
    repo: 'acme/platform',
    runs: 0,
    updated: 600,
  }),
  missionOf({
    path: 'platform/builders/disk-pressure',
    title: 'platform: builder disk pressure',
    goal: 'Keep builder free space above 15% without deleting live GC roots.',
    state: 'cancelled',
    mustAct: 'none',
    repo: 'acme/platform',
    runs: 1,
    updated: 5760,
  }),
  missionOf({
    path: 'docs/runbooks-refresh',
    title: 'docs: refresh on-call runbooks',
    goal: 'Bring every on-call runbook in line with the current gateway and builder topology.',
    state: 'running',
    mustAct: 'human',
    repo: 'acme/docs',
    runs: 1,
    updated: 15,
  }),
]

// ── Decision escalation (D12) ─────────────────────────────────────────────────────────────────

/** One decision alternative and its operator-facing tradeoff. */
export interface WorldDecisionOption {
  readonly id: string
  readonly label: string
  readonly tradeoff: string
}

/** Escalated question linking an operator, mission and blocked agents. */
export interface WorldDecision {
  /** Decision id within the mission's decision log. */
  readonly id: string
  /** `attention/<ulid>` */
  readonly ref: string
  readonly mission: string
  readonly step: string
  readonly raisedBy: string
  readonly owner: string
  readonly title: string
  readonly question: string
  readonly options: readonly WorldDecisionOption[]
  readonly recommendation: string
  readonly raisedAt: string
  readonly blocks: readonly string[]
}

/** The open escalation that parks `MissionsUi` and puts `webfractal/foundation` on the human. */
export const decisionD12: WorldDecision = {
  id: 'D12',
  ref: 'attention/01JDQ7RT4D12',
  mission: 'mission/webfractal/foundation',
  step: 'missions-collection',
  raisedBy: 'agent/build-host-a/missions-ui',
  owner: operator.ref,
  title: 'D12 · Virtualize long collections with React Aria Virtualizer?',
  question:
    'Missions, agents and the conversation list exceed 5k rows on busy fleets. Do we virtualize through React Aria’s Virtualizer (keeps ListBox/GridList semantics) or hand-roll windowing over plain divs?',
  options: [
    {
      id: 'A',
      label: 'React Aria Virtualizer',
      tradeoff: 'Keeps keyboard + screen-reader semantics; ~1.4 ms/frame at 10k rows in the bench.',
    },
    {
      id: 'B',
      label: 'Hand-rolled windowing',
      tradeoff:
        '~0.9 ms/frame, but focus management, typeahead and ARIA set sizes must be re-implemented.',
    },
    {
      id: 'C',
      label: 'Paginate, no virtualization',
      tradeoff: 'Simplest; breaks continuous scroll and keyboard paging across pages.',
    },
  ],
  recommendation: 'A',
  raisedAt: minutesAgo(9),
  blocks: ['agent/build-host-a/missions-ui'],
}

// ── Pull requests and CI ──────────────────────────────────────────────────────────────────────

/** Pull request lifecycle states available to fixture views. */
export type PullRequestState = 'open' | 'draft' | 'merged' | 'closed'
/** Shared check outcome vocabulary for CI runs and jobs. */
export type CheckConclusion = 'success' | 'failure' | 'pending' | 'skipped'

/** Pull request metadata linked to its authoring agent and mission. */
export interface WorldPullRequest {
  readonly repo: RepoId
  readonly number: number
  /** `resource/github/<repo>/pull/<n>` */
  readonly ref: string
  readonly url: string
  readonly title: string
  readonly state: PullRequestState
  readonly author: string
  readonly agent: string
  readonly mission: string
  readonly branch: string
  readonly headSha: string
  readonly additions: number
  readonly deletions: number
  readonly reviews: ReadonlyArray<{
    readonly reviewer: string
    readonly state: 'approved' | 'changes_requested' | 'commented' | 'pending'
  }>
  readonly openedAt: string
  readonly updatedAt: string
}

const pr = (
  spec: Omit<WorldPullRequest, 'ref' | 'url' | 'author' | 'branch' | 'openedAt' | 'updatedAt'> & {
    readonly opened: number
    readonly updated: number
  },
): WorldPullRequest => {
  const { opened, updated, ...rest } = spec
  const owner = agents.find((a) => a.ref === spec.agent)
  return {
    ...rest,
    ref: `resource/github/${spec.repo}/pull/${spec.number}`,
    url: `https://github.com/${spec.repo}/pull/${spec.number}`,
    author: botAccount,
    branch: owner?.session.branch ?? 'main',
    openedAt: minutesAgo(opened),
    updatedAt: minutesAgo(updated),
  }
}

/** Pull requests exercising open, draft and merged resource views. */
export const pullRequests: readonly WorldPullRequest[] = [
  pr({
    repo: 'acme/webfractal',
    number: 214,
    title: 'feat(shell): workbench with dock, editor groups and quick open',
    state: 'open',
    agent: 'agent/build-host-a/workbench-shell',
    mission: 'mission/webfractal/foundation',
    headSha: '9f3c2a17be04d1e5a6c8',
    additions: 2841,
    deletions: 312,
    reviews: [{ reviewer: reviewer.handle, state: 'changes_requested' }],
    opened: 160,
    updated: 3,
  }),
  pr({
    repo: 'acme/webfractal',
    number: 219,
    title: 'feat(terminal): ghostty-web renderer behind TerminalPane',
    state: 'merged',
    agent: 'agent/build-host-a/terminal-renderer',
    mission: 'mission/webfractal/terminal-renderer',
    headSha: '2b71d0c9a4e8f36015bd',
    additions: 1204,
    deletions: 96,
    reviews: [{ reviewer: operator.handle, state: 'approved' }],
    opened: 1620,
    updated: 1440,
  }),
  pr({
    repo: 'acme/webfractal',
    number: 221,
    title: 'feat(palette): subject actions with fenced confirmation',
    state: 'open',
    agent: 'agent/build-host-a/palette-review',
    mission: 'mission/webfractal/command-palette',
    headSha: 'c40e8a2f71b95d3e0a6f',
    additions: 932,
    deletions: 141,
    reviews: [{ reviewer: operator.handle, state: 'pending' }],
    opened: 95,
    updated: 22,
  }),
  pr({
    repo: 'acme/gateway',
    number: 88,
    title: 'feat(client): freeze st3.client v1 envelopes',
    state: 'draft',
    agent: 'agent/build-host-a/gateway-schema',
    mission: 'mission/gateway/schema-v1',
    headSha: '71aa3e0d5c92b8f4e1c7',
    additions: 3310,
    deletions: 2875,
    reviews: [],
    opened: 120,
    updated: 1,
  }),
  pr({
    repo: 'acme/platform',
    number: 1312,
    title: 'chore(deps): weekly external flake bumps (w40)',
    state: 'open',
    agent: 'agent/build-host-b/deps-steward',
    mission: 'mission/platform/deps/weekly-cycle',
    headSha: 'e8d4019b6a37c2f5d0e9',
    additions: 64,
    deletions: 58,
    reviews: [],
    opened: 140,
    updated: 2.5,
  }),
  pr({
    repo: 'acme/platform',
    number: 1309,
    title: 'feat(cas): stream build artefacts into the content-addressed store',
    state: 'open',
    agent: 'agent/build-host-b/cas-janitor',
    mission: 'mission/platform/storage/cas-migration',
    headSha: '5c1f7e82d09a4b36e2a1',
    additions: 778,
    deletions: 203,
    reviews: [{ reviewer: operator.handle, state: 'approved' }],
    opened: 2100,
    updated: 30,
  }),
  pr({
    repo: 'acme/platform',
    number: 1315,
    title: 'ci: quarantine flaky darwin-activation check',
    state: 'draft',
    agent: 'agent/build-host-b/ci-doctor',
    mission: 'mission/platform/ci/flake-triage',
    headSha: 'a93b6c04e1f2d7850c3e',
    additions: 37,
    deletions: 4,
    reviews: [],
    opened: 18,
    updated: 0.5,
  }),
  pr({
    repo: 'acme/docs',
    number: 57,
    title: 'docs(runbooks): gateway restart and builder disk pressure',
    state: 'open',
    agent: 'agent/build-host-b/docs-writer',
    mission: 'mission/docs/runbooks-refresh',
    headSha: '0d6e2b91c8a7f4e3b5d2',
    additions: 412,
    deletions: 289,
    reviews: [{ reviewer: reviewer.handle, state: 'pending' }],
    opened: 40,
    updated: 15,
  }),
]

/** Workflow execution and job outcomes linked to a pull request. */
export interface WorldCiRun {
  readonly repo: RepoId
  readonly id: number
  /** `resource/github/<repo>/actions/run/<id>` */
  readonly ref: string
  readonly url: string
  readonly pullRequest: string
  readonly workflow: string
  readonly headSha: string
  readonly status: 'queued' | 'in_progress' | 'completed'
  readonly conclusion: CheckConclusion
  readonly jobs: ReadonlyArray<{
    readonly name: string
    readonly conclusion: CheckConclusion
    readonly durationS: number
  }>
  readonly startedAt: string
  readonly updatedAt: string
}

const ciRun = (
  spec: Omit<WorldCiRun, 'ref' | 'url' | 'repo' | 'headSha' | 'startedAt' | 'updatedAt'> & {
    readonly started: number
    readonly updated: number
  },
): WorldCiRun => {
  const { started, updated, ...rest } = spec
  const pull = pullRequests.find((p) => p.ref === spec.pullRequest)
  const repoId = pull?.repo ?? 'acme/platform'
  return {
    ...rest,
    repo: repoId,
    ref: `resource/github/${repoId}/actions/run/${spec.id}`,
    url: `https://github.com/${repoId}/actions/runs/${spec.id}`,
    headSha: pull?.headSha ?? '',
    startedAt: minutesAgo(started),
    updatedAt: minutesAgo(updated),
  }
}

/** CI executions covering queued, running, successful and failed checks. */
export const ciRuns: readonly WorldCiRun[] = [
  ciRun({
    id: 11873452,
    pullRequest: 'resource/github/acme/webfractal/pull/214',
    workflow: 'ci',
    status: 'in_progress',
    conclusion: 'pending',
    jobs: [
      { name: 'typecheck', conclusion: 'success', durationS: 94 },
      { name: 'storybook build', conclusion: 'pending', durationS: 0 },
      { name: 'playwright', conclusion: 'pending', durationS: 0 },
    ],
    started: 3,
    updated: 0.5,
  }),
  ciRun({
    id: 11870118,
    pullRequest: 'resource/github/acme/webfractal/pull/221',
    workflow: 'ci',
    status: 'completed',
    conclusion: 'success',
    jobs: [
      { name: 'typecheck', conclusion: 'success', durationS: 88 },
      { name: 'storybook build', conclusion: 'success', durationS: 212 },
      { name: 'playwright', conclusion: 'success', durationS: 341 },
    ],
    started: 30,
    updated: 24,
  }),
  ciRun({
    id: 11866703,
    pullRequest: 'resource/github/acme/platform/pull/1312',
    workflow: 'check',
    status: 'completed',
    conclusion: 'failure',
    jobs: [
      { name: 'nix flake check', conclusion: 'success', durationS: 611 },
      { name: 'darwin-activation', conclusion: 'failure', durationS: 1287 },
      { name: 'eval-hosts', conclusion: 'success', durationS: 402 },
    ],
    started: 70,
    updated: 48,
  }),
  ciRun({
    id: 11861290,
    pullRequest: 'resource/github/acme/gateway/pull/88',
    workflow: 'rust',
    status: 'queued',
    conclusion: 'pending',
    jobs: [
      { name: 'cargo test', conclusion: 'pending', durationS: 0 },
      { name: 'client codegen drift', conclusion: 'pending', durationS: 0 },
    ],
    started: 1,
    updated: 1,
  }),
]

// ── Quota accounts (usage hub) ────────────────────────────────────────────────────────────────

/** Provider quota consumption and reset time for one billing window. */
export interface WorldQuotaWindow {
  readonly window: '5h' | '7d'
  /** Used fraction, 0..1. */
  readonly used: number
  readonly resetsAt: string
}

/** Billing account and the agent sessions consuming its quota. */
export interface WorldQuotaAccount {
  readonly id: string
  readonly provider: 'anthropic' | 'openai'
  readonly plan: string
  readonly label: string
  /** Agent refs whose harness bills this account. */
  readonly agents: readonly string[]
  readonly windows: readonly WorldQuotaWindow[]
}

const billedBy = ({
  harnesses,
  hostIds,
}: {
  harnesses: readonly Harness[]
  hostIds: readonly HostId[]
}) =>
  agents
    .filter((a) => harnesses.includes(a.session.harness) && hostIds.includes(a.host))
    .map((a) => a.ref)

/** Provider accounts with deterministic billing assignments and quota windows. */
export const quotaAccounts: readonly WorldQuotaAccount[] = [
  {
    id: 'anthropic-max-a',
    provider: 'anthropic',
    plan: 'Max 20×',
    label: 'Anthropic · fleet A',
    agents: billedBy({ harnesses: ['omp', 'claude'], hostIds: ['build-host-a'] }),
    windows: [
      { window: '5h', used: 0.82, resetsAt: minutesAgo(-68) },
      { window: '7d', used: 0.61, resetsAt: daysAgo(-3.2) },
    ],
  },
  {
    id: 'anthropic-max-b',
    provider: 'anthropic',
    plan: 'Max 20×',
    label: 'Anthropic · fleet B',
    agents: billedBy({ harnesses: ['omp', 'claude'], hostIds: ['build-host-b', 'build-host-c'] }),
    windows: [
      { window: '5h', used: 0.34, resetsAt: minutesAgo(-191) },
      { window: '7d', used: 0.47, resetsAt: daysAgo(-3.2) },
    ],
  },
  {
    id: 'openai-pro',
    provider: 'openai',
    plan: 'Pro',
    label: 'OpenAI · codex',
    agents: billedBy({
      harnesses: ['codex'],
      hostIds: ['build-host-a', 'build-host-b', 'build-host-c'],
    }),
    windows: [
      { window: '5h', used: 0.97, resetsAt: minutesAgo(-12) },
      { window: '7d', used: 0.73, resetsAt: daysAgo(-5.6) },
    ],
  },
]

// ── Vista publications ────────────────────────────────────────────────────────────────────────

/** Versioned Vista publication linked to its publishing agent. */
export interface WorldVista {
  /** Publishing agent's host-qualified identity. */
  readonly owner: string
  readonly slug: string
  readonly version: number
  readonly title: string
  readonly agent: string
  readonly mission?: string
  readonly publishedAt: string
}

/** Base URL of the Vista server publications are served from. */
export const vistaBase = 'https://vista.acme.dev'

/** Published apps available to fixture resource and Vista views. */
export const vistas: readonly WorldVista[] = [
  {
    owner: 'build-host-a.direct.omp.k3v9q2xm',
    slug: 'workbench-layout-review',
    version: 3,
    title: 'Workbench layout review',
    agent: 'agent/build-host-a/workbench-shell',
    mission: 'mission/webfractal/foundation',
    publishedAt: minutesAgo(64),
  },
  {
    owner: 'build-host-a.direct.omp.r7t2w8cn',
    slug: 'stylex-probe',
    version: 1,
    title: 'StyleX probe',
    agent: 'agent/build-host-a/terminal-renderer',
    mission: 'mission/webfractal/vista-embed',
    publishedAt: daysAgo(5.4),
  },
  {
    owner: 'build-host-b.direct.omp.h4q8z1dv',
    slug: 'closure-size-map',
    version: 2,
    title: 'Builder closure — size map',
    agent: 'agent/build-host-b/deps-steward',
    mission: 'mission/platform/deps/weekly-cycle',
    publishedAt: hoursAgo(25.5),
  },
]

/** Public URL of a Vista publication version. */
export const vistaUrl = (vista: WorldVista) =>
  `${vistaBase}/r/${vista.owner}/${vista.slug}/v${vista.version}/`

// ── Gateway events ────────────────────────────────────────────────────────────────────────────

/** Timestamped gateway or host activity for the status panel. */
export interface WorldEvent {
  readonly at: string
  readonly source: HostId | 'gateway'
  readonly text: string
}

/** Most recent first; what the status events panel shows. */
export const events: readonly WorldEvent[] = [
  {
    at: clockAgo(0.2),
    source: 'build-host-b',
    text: 'CiDoctor tool call: nix build .#checks.darwin-activation (exit 1)',
  },
  {
    at: clockAgo(0.4),
    source: 'build-host-a',
    text: 'WorkbenchShell tool call: storybook build (exit 0)',
  },
  { at: clockAgo(1.2), source: 'build-host-a', text: 'TerminalRenderer pty resized 200×60' },
  {
    at: clockAgo(4),
    source: 'build-host-b',
    text: 'CasJanitor faulted: ENOSPC on /srv/cas-staging',
  },
  { at: clockAgo(9), source: 'build-host-a', text: 'MissionsUi escalated D12 to Sam Okafor' },
  { at: clockAgo(12), source: 'gateway', text: 'collections socket reconnected (follows 5/8)' },
  { at: clockAgo(47), source: 'gateway', text: 'build-host-c heartbeat lost' },
]

// ── Lookups and invariants ────────────────────────────────────────────────────────────────────

/** Finds a fleet host by its public-safe identifier. */
export const hostById = (id: HostId) => hosts.find((h) => h.id === id)
/** Finds an agent by either its agent reference or terminal reference. */
export const agentByRef = (ref: string) => agents.find((a) => a.ref === ref || a.terminal === ref)
/** Finds a mission by its canonical resource reference. */
export const missionByRef = (ref: string) => missions.find((m) => m.ref === ref)
/** Finds a pull request by its canonical resource reference. */
export const pullRequestByRef = (ref: string) => pullRequests.find((p) => p.ref === ref)
/** Lists agents assigned to a particular fixture host. */
export const agentsOnHost = (id: HostId) => agents.filter((a) => a.host === id)

/** The whole world as one value, for the `DataSource` seam. */
export const world = {
  now: worldNow,
  hosts,
  gatewayHost,
  operator,
  reviewer,
  repos,
  agents,
  missions,
  decisions: [decisionD12],
  pullRequests,
  ciRuns,
  quotaAccounts,
  vistas,
  events,
} as const

/** Type of the shared deterministic fixture identity graph. */
export type World = typeof world

const assertWorld = () => {
  const missionRefs = new Set(missions.map((m) => m.ref))
  const agentRefs = new Set(agents.map((a) => a.ref))
  const problems = [
    ...agents
      .filter((a) => a.mission !== undefined && !missionRefs.has(a.mission))
      .map((a) => `${a.ref} → ${a.mission}`),
    ...pullRequests
      .filter((p) => !missionRefs.has(p.mission) || !agentRefs.has(p.agent))
      .map((p) => `${p.ref} dangling`),
    ...ciRuns
      .filter((c) => pullRequestByRef(c.pullRequest) === undefined)
      .map((c) => `${c.ref} → ${c.pullRequest}`),
    ...vistas
      .filter(
        (v) => !agentRefs.has(v.agent) || (v.mission !== undefined && !missionRefs.has(v.mission)),
      )
      .map((v) => `vista ${v.slug} dangling`),
    ...(missionRefs.has(decisionD12.mission) && agentRefs.has(decisionD12.raisedBy)
      ? []
      : ['D12 dangling']),
  ]
  if (problems.length > 0) throw new Error(`fixture world is inconsistent: ${problems.join('; ')}`)
}
assertWorld()
