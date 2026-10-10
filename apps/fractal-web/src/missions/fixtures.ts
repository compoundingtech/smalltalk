import {
  Attention,
  Mission,
  decodeUnknownSync,
  type MissionEncoded,
  type MissionRunSummaryEncoded,
  type MissionStepEncoded,
  type MustActEncoded,
  type UsageSummaryEncoded,
  type VisualizationEncoded,
  type WorkState,
} from '@smalltalk/st3-client/schema'
import { Option } from 'effect'

/**
 * Missions fixtures: the fixture world's 15 missions (`src/fixtures/world.ts`) projected into the
 * st3.client.v0 mission and attention resources the missions views read.
 *
 * Identity, title, state, must-act, total run count, the agents that claim steps
 * and every timestamp come from the world; this module adds what the world does not model: run and
 * step structure, visualization graphs, usage and the not-in-protocol proposals. Every `Mission`
 * below is a *detail read* (every detailed run carries its steps); `windowRow` trims it to what the
 * `missions` collection window serves.
 */
import {
  agents,
  decisionD12,
  minutesAgo,
  missions as worldMissions,
  operator,
  worldNow,
  type WorldMission,
} from '../fixtures/world.ts'
import type { ProposedMissionFields } from './model.ts'

/** The world clock; kept under this name for the shell's missions feature. */
export const fixtureNow = worldNow

// --- world lookups ------------------------------------------------------------------------------

const worldMission = (path: string): WorldMission => {
  const found = worldMissions.find((m) => m.path === path)
  if (found === undefined) throw new Error(`missions fixtures: no world mission ${path}`)
  return found
}

/** `agent/<host>/<slug>` of a world agent. */
const seat = (slug: string): string => {
  const found = agents.find((a) => a.slug === slug)
  if (found === undefined) throw new Error(`missions fixtures: no world agent ${slug}`)
  return found.ref
}

const nowMinuteOfDay = (worldNow % 86_400_000) / 60_000

/** ISO timestamp at `HH:MM` UTC, `days` calendar days before the world's current day. */
const onDay = ({ days, time }: { readonly days: number; readonly time: string }): string => {
  const [hours = 0, minutes = 0] = time.split(':').map(Number)
  return minutesAgo(days * 1440 + nowMinuteOfDay - (hours * 60 + minutes))
}

const stateOf: Readonly<Record<WorldMission['state'], MissionEncoded['state']>> = {
  draft: 'draft',
  proposed: 'ready',
  active: 'standing',
  running: 'running',
  paused: 'running',
  completed: 'completed',
  failed: 'failed',
  cancelled: 'cancelled',
}

const mustActOf: Readonly<Record<WorldMission['mustAct'], MustActEncoded>> = {
  human: 'you',
  agent: 'agent',
  none: 'nobody',
}

/** Token usage summed over the world sessions of the given agents. */
const usageOf = ({
  slugs,
  cost,
}: {
  readonly slugs: readonly string[]
  readonly cost: number
}): UsageSummaryEncoded => {
  const sessions = slugs.map((slug) => agents.find((a) => a.slug === slug)?.session)
  const sum = (pick: (tokens: { input: number; output: number; cached: number }) => number) =>
    sessions.reduce((total, session) => total + (session ? pick(session.tokens) : 0), 0)
  const input = sum((t) => t.input)
  const output = sum((t) => t.output)
  return {
    aggregation: 'cumulative-per-incarnation-else-response-deltas',
    input_tokens: input,
    output_tokens: output,
    cached_tokens: sum((t) => t.cached),
    total_tokens: input + output,
    incarnation_count: slugs.length,
    cost,
    currency: 'USD',
  }
}

// --- builders -----------------------------------------------------------------------------------

interface StepSpec {
  readonly path: string
  readonly state: WorkState
  readonly since: string
  readonly extra?: Partial<MissionStepEncoded>
}

const steps = ({
  generation,
  specs,
}: {
  readonly generation: string
  readonly specs: readonly StepSpec[]
}): MissionStepEncoded[] =>
  specs.map(({ path, state, since, extra }) => ({
    id: `step-run/${generation}/${path}`,
    path,
    state,
    since,
    attempt: 1,
    ...extra,
  }))

interface RunSpec {
  readonly n: number
  readonly status: string
  readonly phase: string
  readonly mustAct: MustActEncoded
  readonly since: string
  readonly requester: string
  readonly steps: readonly StepSpec[]
  readonly lastProgress?: string
  readonly outcome?: MissionRunSummaryEncoded['outcome']
}

/** Builds one run summary the way st joins it: progress and current steps derive from the steps. */
const run = ({
  missionPath,
  spec,
}: {
  readonly missionPath: string
  readonly spec: RunSpec
}): MissionRunSummaryEncoded => {
  const generation = `${missionPath}/${spec.n}/g1`
  const runSteps = steps({ generation, specs: spec.steps })
  const done = runSteps.filter((step) => step.state === 'completed').length
  return {
    id: `mission-run/${missionPath}/${spec.n}`,
    generation_id: `run-generation/${generation}`,
    status: spec.status,
    phase: spec.phase,
    progress: { done, total: runSteps.length },
    must_act: spec.mustAct,
    requester: spec.requester,
    state_since: spec.since,
    last_progress: spec.lastProgress ?? null,
    current_steps: runSteps
      .filter((step) => ['claimed', 'ready', 'blocked', 'verifying'].includes(step.state))
      .map((step) => ({
        id: step.id,
        title: step.title ?? null,
        state: step.state,
        since: step.since,
        assignee: step.assignee ?? null,
        claimant: step.claimant ?? null,
      })),
    outcome: spec.outcome ?? null,
    steps: runSteps,
  }
}

/** Step id of step `path` in run `n` of the mission at `missionPath`. */
const stepId = ({
  missionPath,
  n,
  path,
}: {
  readonly missionPath: string
  readonly n: number
  readonly path: string
}) => `step-run/${missionPath}/${n}/g1/${path}`

/** Visualization graph of a world mission from `[path, goal, timeoutH?]` nodes and `[from, to]` edges. */
const graph = ({
  mission,
  nodes,
  edges,
  constraints = [],
}: {
  readonly mission: WorldMission
  readonly nodes: ReadonlyArray<readonly [path: string, goal: string, timeoutH?: number]>
  readonly edges: ReadonlyArray<readonly [from: string, to: string]>
  readonly constraints?: readonly string[]
}): VisualizationEncoded => ({
  version: 'st3.visualization.v0',
  views: ['graph', 'timeline', 'live-progress'],
  mission: mission.ref,
  decisions: [],
  diffs: [],
  gates: [],
  groups: [],
  live_progress: {},
  resources: [],
  revision: {},
  risk: {},
  swimlanes: [],
  timeline: { entries: [] },
  nodes: nodes.map(([path, goal, timeoutH]) => ({
    id: path,
    kind: 'step',
    label: path,
    path,
    goals: [goal],
    constraints: [],
    runtime: {},
    timeout_ms: timeoutH === undefined ? null : timeoutH * 3_600_000,
  })),
  edges: edges.map(([from, to]) => ({
    id: `${from}->${to}`,
    from,
    to,
    kind: 'dependency',
    gate: null,
  })),
  goals: [mission.goal],
  constraints,
})

interface MissionSpec {
  readonly path: string
  /** Detailed runs, oldest first; the last one is the world's latest run. */
  readonly runs: readonly RunSpec[]
  readonly usage?: UsageSummaryEncoded
  readonly visualization?: (mission: WorldMission) => VisualizationEncoded
  readonly revision?: string
}

const mission = (spec: MissionSpec): Mission => {
  const source = worldMission(spec.path)
  const details = spec.runs.map((r) => run({ missionPath: spec.path, spec: r }))
  const latest = spec.runs.at(-1)?.n ?? 0
  if (latest !== source.runs)
    throw new Error(
      `missions fixtures: ${spec.path} latest run ${latest} ≠ world runs ${source.runs}`,
    )
  const earlier = Array.from(
    { length: source.runs - details.length },
    (_, i) => `mission-run/${spec.path}/${i + 1}`,
  )
  const runIds = [...earlier, ...details.map((d) => d.id)]
  return decodeUnknownSync(
    Mission,
    'strict',
  )({
    id: source.ref,
    kind: 'mission',
    revision: `r-${spec.path.length}${spec.runs.length}`,
    updated_at: source.updatedAt,
    title: source.title,
    state: stateOf[source.state],
    mission_revision: spec.revision ?? `sha256:${spec.path.replaceAll('/', '').slice(0, 10)}`,
    must_act: mustActOf[source.mustAct],
    active_runs: details.filter((d) => !['completed', 'failed', 'cancelled'].includes(d.status))
      .length,
    runs: runIds,
    run_generations: Object.fromEntries(
      details.map((d) => [d.id, d.generation_id ?? `run-generation/${d.id}`]),
    ),
    run_details: details,
    usage: spec.usage ?? null,
    ...(spec.visualization ? { visualization: spec.visualization(source) } : {}),
  } satisfies MissionEncoded)
}

// --- webfractal/foundation: multi-seat run parked on decision D12 -------------------------------

const workbenchShell = seat('workbench-shell')
const terminalRenderer = seat('terminal-renderer')
const missionsUi = seat('missions-ui')

/** `webfractal/foundation`: run 3 has two seats working while the missions step waits on decision D12. */
export const webfractalFoundation = mission({
  path: 'webfractal/foundation',
  revision: 'sha256:4be91c07d3',
  usage: usageOf({ slugs: ['workbench-shell', 'terminal-renderer', 'missions-ui'], cost: 38.6 }),
  visualization: (m) =>
    graph({
      mission: m,
      nodes: [
        ['skeleton', 'App skeleton, Storybook and the shared fixture world'],
        [
          'workbench-shell',
          'Workbench shell: dock, editor groups, quick open (acme/webfractal#214)',
        ],
        ['terminal', 'TerminalPane on the bake-off winner, wired to agent PTYs'],
        ['missions-collection', 'Missions collection and detail on st3.client.v0 resources'],
        ['integrate', 'Integrate sessions, missions and resources against the st gateway'],
      ],
      edges: [
        ['skeleton', 'workbench-shell'],
        ['skeleton', 'terminal'],
        ['skeleton', 'missions-collection'],
        ['workbench-shell', 'integrate'],
        ['terminal', 'integrate'],
        ['missions-collection', 'integrate'],
      ],
      constraints: ['React Aria for every interactive control; design tokens through StyleX.'],
    }),
  runs: [
    {
      n: 2,
      status: 'completed',
      phase: 'completed',
      mustAct: 'nobody',
      since: onDay({ days: 2, time: '18:40' }),
      requester: operator.ref,
      lastProgress:
        'Skeleton and Storybook landed; renderer bake-offs handed to their own missions.',
      steps: [
        {
          path: 'skeleton',
          state: 'completed',
          since: onDay({ days: 2, time: '18:40' }),
          extra: { claimant: workbenchShell },
        },
      ],
    },
    {
      n: 3,
      status: 'running',
      phase: decisionD12.step,
      mustAct: 'you',
      since: decisionD12.raisedAt,
      requester: operator.ref,
      lastProgress: `MissionsUi escalated ${decisionD12.id} to ${operator.name}.`,
      steps: [
        {
          path: 'skeleton',
          state: 'completed',
          since: minutesAgo(205),
          extra: { claimant: workbenchShell },
        },
        {
          path: 'workbench-shell',
          state: 'claimed',
          since: minutesAgo(160),
          extra: {
            assignee: workbenchShell,
            claimant: workbenchShell,
            last_progress:
              'acme/webfractal#214: changes requested by pnatarajan; storybook build running',
          },
        },
        {
          path: 'terminal',
          state: 'claimed',
          since: minutesAgo(188),
          extra: {
            assignee: terminalRenderer,
            claimant: terminalRenderer,
            last_progress: 'benchmarking ghostty-web frames',
          },
        },
        {
          path: decisionD12.step,
          state: 'blocked',
          since: decisionD12.raisedAt,
          extra: {
            assignee: missionsUi,
            claimant: missionsUi,
            blocked_reason: `waiting on decision ${decisionD12.id}: ${decisionD12.title.replace(/^D12 · /, '')}`,
            blockers: [decisionD12.ref],
            last_progress:
              'Collection renders 15 missions; 5k-row path needs a virtualization decision',
          },
        },
        { path: 'integrate', state: 'waiting', since: minutesAgo(212) },
      ],
    },
  ],
})

// --- webfractal/command-palette -----------------------------------------------------------------

const paletteReview = seat('palette-review')

/** `webfractal/command-palette`: run 2 in agent review. */
export const commandPalette = mission({
  path: 'webfractal/command-palette',
  usage: usageOf({ slugs: ['palette-review'], cost: 3.9 }),
  visualization: (m) =>
    graph({
      mission: m,
      nodes: [
        ['subjects', 'Index commands, subjects and subject actions'],
        ['fenced-dispatch', 'Confirm fenced actions before dispatch'],
        ['review', 'Review acme/webfractal#221 and land it'],
      ],
      edges: [
        ['subjects', 'fenced-dispatch'],
        ['fenced-dispatch', 'review'],
      ],
    }),
  runs: [
    {
      n: 2,
      status: 'running',
      phase: 'review',
      mustAct: 'agent',
      since: minutesAgo(22),
      requester: operator.ref,
      lastProgress: 'Review posted on acme/webfractal#221; ci run 11870118 green.',
      steps: [
        {
          path: 'subjects',
          state: 'completed',
          since: minutesAgo(90),
          extra: { claimant: paletteReview },
        },
        {
          path: 'fenced-dispatch',
          state: 'completed',
          since: minutesAgo(60),
          extra: { claimant: paletteReview },
        },
        {
          path: 'review',
          state: 'verifying',
          since: minutesAgo(22),
          extra: {
            assignee: paletteReview,
            claimant: paletteReview,
            last_progress: 'Review posted on acme/webfractal#221',
          },
        },
      ],
    },
  ],
})

// --- completed webfractal missions --------------------------------------------------------------

/** `webfractal/terminal-renderer`: completed renderer bake-off. */
export const terminalRendererBakeoff = mission({
  path: 'webfractal/terminal-renderer',
  usage: usageOf({ slugs: ['terminal-renderer'], cost: 6.2 }),
  visualization: (m) =>
    graph({
      mission: m,
      nodes: [
        ['record', 'Record agent sessions as frame fixtures'],
        ['bench', 'Measure frame cost per renderer and size'],
        ['decide', 'Pick the renderer and land it behind TerminalPane'],
      ],
      edges: [
        ['record', 'bench'],
        ['bench', 'decide'],
      ],
    }),
  runs: [
    {
      n: 1,
      status: 'completed',
      phase: 'completed',
      mustAct: 'nobody',
      since: minutesAgo(1440),
      requester: operator.ref,
      lastProgress: 'Merged acme/webfractal#219: ghostty-web renderer behind TerminalPane.',
      steps: [
        {
          path: 'record',
          state: 'completed',
          since: minutesAgo(1900),
          extra: { claimant: terminalRenderer },
        },
        {
          path: 'bench',
          state: 'completed',
          since: minutesAgo(1700),
          extra: { claimant: terminalRenderer },
        },
        {
          path: 'decide',
          state: 'completed',
          since: minutesAgo(1440),
          extra: { claimant: terminalRenderer },
        },
      ],
    },
  ],
})

/** `webfractal/vista-embed`: completed, without visualization or usage. */
export const vistaEmbed = mission({
  path: 'webfractal/vista-embed',
  runs: [
    {
      n: 2,
      status: 'completed',
      phase: 'completed',
      mustAct: 'nobody',
      since: minutesAgo(2880),
      requester: operator.ref,
      lastProgress:
        'Published Vista apps render in-process; native code needs an explicit trust grant.',
      steps: [
        {
          path: 'trust-policy',
          state: 'completed',
          since: minutesAgo(3300),
          extra: { claimant: terminalRenderer },
        },
        {
          path: 'native-render',
          state: 'completed',
          since: minutesAgo(2880),
          extra: { claimant: terminalRenderer },
        },
      ],
    },
  ],
})

// --- gateway missions ---------------------------------------------------------------------------

const gatewaySchema = seat('gateway-schema')

/** `gateway/schema-v1`: first run in progress with the agent. */
export const gatewaySchemaV1 = mission({
  path: 'gateway/schema-v1',
  usage: usageOf({ slugs: ['gateway-schema'], cost: 5.3 }),
  visualization: (m) =>
    graph({
      mission: m,
      nodes: [
        ['freeze-envelopes', 'Freeze st3.client v1 envelopes'],
        ['regenerate-clients', 'Regenerate the TypeScript and Rust clients', 2],
        ['prove', 'cargo test and client codegen drift on acme/gateway#88', 1],
      ],
      edges: [
        ['freeze-envelopes', 'regenerate-clients'],
        ['regenerate-clients', 'prove'],
      ],
    }),
  runs: [
    {
      n: 1,
      status: 'running',
      phase: 'regenerate-clients',
      mustAct: 'agent',
      since: minutesAgo(120),
      requester: operator.ref,
      lastProgress: 'Draft acme/gateway#88 opened; rust run 11861290 queued.',
      steps: [
        {
          path: 'freeze-envelopes',
          state: 'completed',
          since: minutesAgo(120),
          extra: { claimant: gatewaySchema },
        },
        {
          path: 'regenerate-clients',
          state: 'claimed',
          since: minutesAgo(118),
          extra: {
            assignee: gatewaySchema,
            claimant: gatewaySchema,
            last_progress: 'regenerating client models',
          },
        },
        {
          path: 'prove',
          state: 'waiting',
          since: minutesAgo(141),
          extra: { assignee: gatewaySchema },
        },
      ],
    },
  ],
})

const sdkInteropSeat = seat('sdk-interop')

/** `gateway/sdk-interop`: paused; run 2 blocked on the Darwin step after its host went offline. */
export const sdkInterop = mission({
  path: 'gateway/sdk-interop',
  usage: usageOf({ slugs: ['sdk-interop'], cost: 7.1 }),
  visualization: (m) =>
    graph({
      mission: m,
      nodes: [
        ['linux', 'TypeScript SDK against the Rust gateway on build-host-a', 2],
        ['darwin', 'Same matrix on build-host-c (Darwin)', 2],
        ['report', 'Publish the interop matrix'],
      ],
      edges: [
        ['linux', 'report'],
        ['darwin', 'report'],
      ],
    }),
  runs: [
    {
      n: 2,
      status: 'blocked',
      phase: 'darwin',
      mustAct: 'you',
      since: minutesAgo(47),
      requester: operator.ref,
      lastProgress: 'build-host-c heartbeat lost mid-run; SdkInterop last seen 47m ago.',
      steps: [
        {
          path: 'linux',
          state: 'completed',
          since: minutesAgo(210),
          extra: { claimant: sdkInteropSeat },
        },
        {
          path: 'darwin',
          state: 'blocked',
          since: minutesAgo(47),
          extra: {
            assignee: sdkInteropSeat,
            claimant: sdkInteropSeat,
            blocked_reason: 'host build-host-c offline (heartbeat lost 47m ago)',
            last_progress: '11 of 18 Darwin cases passed before the host dropped',
          },
        },
        { path: 'report', state: 'waiting', since: minutesAgo(290) },
      ],
    },
  ],
})

/** `gateway/rate-limits`: proposed, published but never run. */
export const rateLimits = mission({
  path: 'gateway/rate-limits',
  visualization: (m) =>
    graph({
      mission: m,
      nodes: [
        ['measure', 'Measure live follows per client on busy fleets', 2],
        ['budget', 'Define per-client follow budgets'],
        ['enforce', 'Enforce budgets in the collection socket', 4],
      ],
      edges: [
        ['measure', 'budget'],
        ['budget', 'enforce'],
      ],
    }),
  runs: [],
})

// --- platform/deps/weekly-cycle: the weekly dependency cycle, run 40 in progress ----------------

const depsSteward = seat('deps-steward')

/** Monday `days` back from the world's Thursday, as the weekly schedule fires. */
const weeklyCycleSteps = ({
  daysBack,
  overrides,
}: {
  readonly daysBack: number
  readonly overrides: Partial<Record<string, Pick<StepSpec, 'state' | 'since' | 'extra'>>>
}): StepSpec[] =>
  (['worktree', 'select', 'bump', 'prove', 'judge-and-close'] as const).map(
    (path, i): StepSpec =>
      Object.assign(
        {
          path,
          state: 'completed' as const,
          since: onDay({ days: daysBack, time: `${String(7 + i).padStart(2, '0')}:1${i}` }),
          extra:
            path === 'worktree'
              ? { agentless: true }
              : { assignee: depsSteward, claimant: depsSteward },
        },
        overrides[path],
      ),
  )

/** `platform/deps/weekly-cycle`: standing weekly cycle with run 40 in progress. */
export const weeklyCycle = mission({
  path: 'platform/deps/weekly-cycle',
  revision: 'sha256:7c1e9a0f2b',
  usage: usageOf({ slugs: ['deps-steward'], cost: 11.2 }),
  visualization: (m) =>
    graph({
      mission: m,
      nodes: [
        ['worktree', 'Fresh worktree for the cycle branch'],
        ['select', 'Inspect flakes/external/* versions, release notes and real usage', 1],
        ['bump', 'Update selected external flakes and lock/hash files', 4],
        ['prove', 'Per-flake checks, nix flake check vs main, build-host-b + Darwin builds', 8],
        [
          'judge-and-close',
          'Classify impact; trivial → PR + merge, serious → decision to the operator',
          4,
        ],
      ],
      edges: [
        ['worktree', 'select'],
        ['select', 'bump'],
        ['bump', 'prove'],
        ['prove', 'judge-and-close'],
      ],
      constraints: [
        'Only flakes/external/* and required lock or hash updates are in scope.',
        'Never activate a host, deploy, or claim CI passed because a bypass merged a pull request.',
      ],
    }),
  runs: [
    {
      n: 36,
      status: 'completed',
      phase: 'completed',
      mustAct: 'nobody',
      since: onDay({ days: 31, time: '11:58' }),
      requester: operator.ref,
      lastProgress: 'No outdated flakes/external/* inputs; no pull request opened.',
      steps: weeklyCycleSteps({ daysBack: 31, overrides: {} }),
    },
    {
      n: 37,
      status: 'completed',
      phase: 'completed',
      mustAct: 'nobody',
      since: onDay({ days: 24, time: '11:40' }),
      requester: operator.ref,
      lastProgress: 'Merged mermaid-ascii 0.6.1 → 0.7.0 (trivial bump).',
      steps: weeklyCycleSteps({ daysBack: 24, overrides: {} }),
    },
    {
      n: 38,
      status: 'cancelled',
      phase: 'prove',
      mustAct: 'nobody',
      since: onDay({ days: 17, time: '16:05' }),
      requester: operator.ref,
      lastProgress: 'eval-hosts fails on the cycle branch, passes on main.',
      outcome: {
        status: 'cancelled',
        previous_status: 'failed',
        actor: operator.ref,
        at: onDay({ days: 16, time: '08:30' }),
        reason: 'Superseded: the eval-hosts regression was fixed on main; next cycle retries.',
      },
      steps: weeklyCycleSteps({
        daysBack: 17,
        overrides: {
          prove: {
            state: 'failed',
            since: onDay({ days: 17, time: '16:05' }),
            extra: {
              assignee: depsSteward,
              claimant: depsSteward,
              last_progress: 'eval-hosts: new failure not reproduced on main',
            },
          },
          'judge-and-close': { state: 'cancelled', since: onDay({ days: 16, time: '08:30' }) },
        },
      }),
    },
    {
      n: 39,
      status: 'completed',
      phase: 'completed',
      mustAct: 'nobody',
      since: onDay({ days: 10, time: '14:22' }),
      requester: operator.ref,
      lastProgress: 'Merged three trivial bumps after local proof on build-host-b.',
      steps: weeklyCycleSteps({ daysBack: 10, overrides: {} }),
    },
    {
      n: 40,
      status: 'running',
      phase: 'prove',
      mustAct: 'agent',
      since: minutesAgo(138),
      requester: operator.ref,
      lastProgress: 'Opened acme/platform#1312; check run 11866703 red on darwin-activation.',
      steps: weeklyCycleSteps({
        daysBack: 3,
        overrides: {
          bump: {
            state: 'completed',
            since: minutesAgo(140),
            extra: {
              assignee: depsSteward,
              claimant: depsSteward,
              last_progress: 'Opened acme/platform#1312 (weekly external flake bumps, w40)',
            },
          },
          prove: {
            state: 'claimed',
            since: minutesAgo(138),
            extra: {
              assignee: depsSteward,
              claimant: depsSteward,
              last_progress:
                'darwin-activation red in run 11866703 (known flake, quarantine in acme/platform#1315); re-proving locally',
            },
          },
          'judge-and-close': {
            state: 'waiting',
            since: onDay({ days: 3, time: '07:00' }),
            extra: { assignee: depsSteward },
          },
        },
      }),
    },
  ],
})

// --- platform/health/daily: the standing daily health probe -------------------------------------

const healthProbe = seat('health-probe')

/** The daily probe's steps, each completed at its usual time `daysBack` days ago unless overridden. */
const dailyHealthSteps = ({
  daysBack,
  overrides,
}: {
  readonly daysBack: number
  readonly overrides: Partial<Record<string, Pick<StepSpec, 'state' | 'since' | 'extra'>>>
}): StepSpec[] =>
  (['invariants', 'assess', 'recover-or-escalate'] as const).map(
    (path, i): StepSpec =>
      Object.assign(
        {
          path,
          state: 'completed' as const,
          since: onDay({ days: daysBack, time: `0${6 + i}:2${i}` }),
          extra: { assignee: healthProbe, claimant: healthProbe },
        },
        overrides[path],
      ),
  )

/** `platform/health/daily`: standing daily health probe waiting on the person. */
export const dailyHealth = mission({
  path: 'platform/health/daily',
  revision: 'sha256:3fe0b18d4c',
  usage: usageOf({ slugs: ['health-probe'], cost: 18.4 }),
  visualization: (m) =>
    graph({
      mission: m,
      nodes: [
        ['invariants', 'Run the steady-state invariant probe on every host', 1],
        ['assess', 'Gateway doctor, stuck runs, main CI, own merge receipts vs prior reports', 3],
        [
          'recover-or-escalate',
          'Formatter-only baseline fixes; revert own breakage; else a finding',
          6,
        ],
      ],
      edges: [
        ['invariants', 'assess'],
        ['assess', 'recover-or-escalate'],
      ],
      constraints: ['Do not perform general incident triage, reset main, or activate a host.'],
    }),
  runs: [
    {
      n: 270,
      status: 'completed',
      phase: 'completed',
      mustAct: 'nobody',
      since: onDay({ days: 4, time: '09:02' }),
      requester: operator.ref,
      lastProgress: 'Healthy; unchanged findings stay silent.',
      steps: dailyHealthSteps({ daysBack: 4, overrides: {} }),
    },
    {
      n: 271,
      status: 'completed',
      phase: 'completed',
      mustAct: 'nobody',
      since: onDay({ days: 3, time: '09:10' }),
      requester: operator.ref,
      lastProgress: 'Healthy.',
      steps: dailyHealthSteps({ daysBack: 3, overrides: {} }),
    },
    {
      n: 272,
      status: 'failed',
      phase: 'invariants',
      mustAct: 'nobody',
      since: onDay({ days: 2, time: '08:21' }),
      requester: operator.ref,
      lastProgress:
        'Invariant probe timed out after 1h: Darwin activation check hung on build-host-c.',
      outcome: {
        status: 'failed',
        actor: 'daemon/system',
        at: onDay({ days: 2, time: '08:21' }),
        reason: 'step invariants exceeded its 1h timeout on attempt 2',
      },
      steps: dailyHealthSteps({
        daysBack: 2,
        overrides: {
          invariants: {
            state: 'failed',
            since: onDay({ days: 2, time: '08:21' }),
            extra: {
              assignee: healthProbe,
              claimant: healthProbe,
              last_progress: 'probe timed out after 1h (attempt 2)',
            },
          },
          assess: { state: 'cancelled', since: onDay({ days: 2, time: '08:21' }) },
          'recover-or-escalate': { state: 'cancelled', since: onDay({ days: 2, time: '08:21' }) },
        },
      }),
    },
    {
      n: 273,
      status: 'completed',
      phase: 'completed',
      mustAct: 'nobody',
      since: onDay({ days: 1, time: '09:58' }),
      requester: operator.ref,
      lastProgress: 'Fixed a formatter-only red baseline on acme/platform main.',
      steps: dailyHealthSteps({ daysBack: 1, overrides: {} }),
    },
    {
      n: 274,
      status: 'completed',
      phase: 'completed',
      mustAct: 'nobody',
      since: minutesAgo(318),
      requester: operator.ref,
      lastProgress:
        'All invariants hold on 3 hosts; proposed revision 4 for a Darwin activation probe.',
      steps: dailyHealthSteps({
        daysBack: 0,
        overrides: {
          'recover-or-escalate': {
            state: 'completed',
            since: minutesAgo(318),
            extra: {
              assignee: healthProbe,
              claimant: healthProbe,
              last_progress: 'No regressions; revision 4 proposed via mission.revise',
            },
          },
        },
      }),
    },
  ],
})

// --- platform/storage/cas-migration: blocked on a human gate after ENOSPC -----------------------

const casJanitor = seat('cas-janitor')
const casPath = 'platform/storage/cas-migration'

/** `platform/storage/cas-migration`: blocked on a human gate after ENOSPC. */
export const casMigration = mission({
  path: casPath,
  usage: usageOf({ slugs: ['cas-janitor'], cost: 4.1 }),
  visualization: (m) =>
    graph({
      mission: m,
      nodes: [
        ['plan', 'Inventory build artefacts referenced by live manifests', 2],
        ['copy-batch-1', 'Stream batch 1 into /srv/cas-staging with content-hash verification', 12],
        ['verify-batch-1', 'Re-hash every copied blob; diff manifests', 4],
        ['delete-originals-batch-1', 'Delete batch 1 originals after operator approval'],
        ['copy-batch-2', 'Stream batch 2 into /srv/cas-staging', 12],
      ],
      edges: [
        ['plan', 'copy-batch-1'],
        ['copy-batch-1', 'verify-batch-1'],
        ['verify-batch-1', 'delete-originals-batch-1'],
        ['verify-batch-1', 'copy-batch-2'],
      ],
      constraints: ['Never delete an original before its blob is re-hashed in the CAS.'],
    }),
  runs: [
    {
      n: 1,
      status: 'blocked',
      phase: 'delete-originals-batch-1',
      mustAct: 'you',
      since: minutesAgo(4),
      requester: operator.ref,
      lastProgress:
        'Batch 1 verified (248,112 blobs, 1.1 TiB); batch 2 hit ENOSPC on /srv/cas-staging.',
      steps: [
        {
          path: 'plan',
          state: 'completed',
          since: minutesAgo(2100),
          extra: { claimant: casJanitor },
        },
        {
          path: 'copy-batch-1',
          state: 'completed',
          since: minutesAgo(900),
          extra: { claimant: casJanitor },
        },
        {
          path: 'verify-batch-1',
          state: 'completed',
          since: minutesAgo(266),
          extra: {
            claimant: casJanitor,
            last_progress: '248,112 blobs re-hashed, zero manifest diffs',
          },
        },
        {
          path: 'delete-originals-batch-1',
          state: 'blocked',
          since: minutesAgo(266),
          extra: {
            agentless: true,
            blocked_reason: 'human gate "operator approves deleting verified originals"',
          },
        },
        {
          path: 'copy-batch-2',
          state: 'failed',
          since: minutesAgo(4),
          extra: {
            assignee: casJanitor,
            claimant: casJanitor,
            last_progress: 'ENOSPC on /srv/cas-staging after 61% of batch 2',
          },
        },
      ],
    },
  ],
})

// --- platform/ci/flake-triage -------------------------------------------------------------------

const ciDoctor = seat('ci-doctor')

/** `platform/ci/flake-triage`: first run in progress with the agent. */
export const flakeTriage = mission({
  path: 'platform/ci/flake-triage',
  usage: usageOf({ slugs: ['ci-doctor'], cost: 2.4 }),
  visualization: (m) =>
    graph({
      mission: m,
      nodes: [
        ['collect', 'Collect checks that failed without a code change'],
        ['bisect', 'Reproduce and bisect each flaky check', 4],
        ['quarantine', 'Quarantine confirmed flakes with an owner'],
      ],
      edges: [
        ['collect', 'bisect'],
        ['bisect', 'quarantine'],
      ],
    }),
  runs: [
    {
      n: 1,
      status: 'running',
      phase: 'bisect',
      mustAct: 'agent',
      since: minutesAgo(74),
      requester: operator.ref,
      lastProgress: 'Draft acme/platform#1315 quarantines darwin-activation.',
      steps: [
        {
          path: 'collect',
          state: 'completed',
          since: minutesAgo(60),
          extra: { claimant: ciDoctor },
        },
        {
          path: 'bisect',
          state: 'claimed',
          since: minutesAgo(58),
          extra: {
            assignee: ciDoctor,
            claimant: ciDoctor,
            last_progress: 'nix build .#checks.darwin-activation exits 1 on 2 of 5 reruns of main',
          },
        },
        {
          path: 'quarantine',
          state: 'ready',
          since: minutesAgo(18),
          extra: {
            assignee: ciDoctor,
            last_progress: 'Draft acme/platform#1315 opened ahead of the bisect result',
          },
        },
      ],
    },
  ],
})

// --- archive: completed and cancelled platform missions -----------------------------------------

/** `platform/secrets/rotation`: completed archive mission. */
export const secretsRotation = mission({
  path: 'platform/secrets/rotation',
  runs: [
    {
      n: 1,
      status: 'completed',
      phase: 'completed',
      mustAct: 'nobody',
      since: minutesAgo(4320),
      requester: operator.ref,
      lastProgress: 'All CI deploy keys rotated; no runner used an old key for 24h.',
      steps: [
        {
          path: 'inventory',
          state: 'completed',
          since: minutesAgo(5800),
          extra: { agentless: true },
        },
        { path: 'rotate', state: 'completed', since: minutesAgo(5700), extra: { agentless: true } },
        {
          path: 'prove-unused',
          state: 'completed',
          since: minutesAgo(4320),
          extra: { agentless: true },
        },
      ],
    },
  ],
})

/** `platform/observability/otel-cutover`: draft, never published or run. */
export const otelCutover = mission({
  path: 'platform/observability/otel-cutover',
  visualization: (m) =>
    graph({
      mission: m,
      nodes: [
        ['inventory', 'List every sidecar exporter and its consumers'],
        [
          'dual-write',
          'Send agent telemetry to the shared OTLP collector alongside the sidecars',
          8,
        ],
        ['retire', 'Retire the sidecar exporters once dashboards read from OTLP'],
      ],
      edges: [
        ['inventory', 'dual-write'],
        ['dual-write', 'retire'],
      ],
    }),
  runs: [],
})

/** `platform/builders/disk-pressure`: cancelled archive mission. */
export const diskPressure = mission({
  path: 'platform/builders/disk-pressure',
  runs: [
    {
      n: 1,
      status: 'cancelled',
      phase: 'prune',
      mustAct: 'nobody',
      since: minutesAgo(5760),
      requester: operator.ref,
      outcome: {
        status: 'cancelled',
        actor: operator.ref,
        at: minutesAgo(5760),
        reason:
          'Superseded by platform/storage/cas-migration: moving artefacts into the CAS frees the space without touching GC roots.',
      },
      steps: [
        {
          path: 'measure',
          state: 'completed',
          since: minutesAgo(6000),
          extra: { agentless: true },
        },
        { path: 'prune', state: 'cancelled', since: minutesAgo(5760) },
      ],
    },
  ],
})

// --- docs/runbooks-refresh ----------------------------------------------------------------------

const docsWriter = seat('docs-writer')

/** `docs/runbooks-refresh`: first run waiting on the person. */
export const runbooksRefresh = mission({
  path: 'docs/runbooks-refresh',
  usage: usageOf({ slugs: ['docs-writer'], cost: 2.9 }),
  visualization: (m) =>
    graph({
      mission: m,
      nodes: [
        ['draft', 'Rewrite gateway restart and builder disk-pressure runbooks'],
        ['confirm-topology', 'Confirm host roles with the operator'],
        ['review', 'Land acme/docs#57 after review'],
      ],
      edges: [
        ['draft', 'confirm-topology'],
        ['confirm-topology', 'review'],
      ],
    }),
  runs: [
    {
      n: 1,
      status: 'blocked',
      phase: 'confirm-topology',
      mustAct: 'you',
      since: minutesAgo(15),
      requester: operator.ref,
      lastProgress: 'Opened acme/docs#57; review requested from pnatarajan.',
      steps: [
        {
          path: 'draft',
          state: 'completed',
          since: minutesAgo(40),
          extra: { claimant: docsWriter },
        },
        {
          path: 'confirm-topology',
          state: 'blocked',
          since: minutesAgo(15),
          extra: {
            assignee: docsWriter,
            claimant: docsWriter,
            blocked_reason: `waiting on ${operator.name}: is build-host-c an on-call builder?`,
          },
        },
        {
          path: 'review',
          state: 'waiting',
          since: minutesAgo(133),
          extra: { assignee: docsWriter },
        },
      ],
    },
  ],
})

// --- attention ----------------------------------------------------------------------------------

const optionsLine = decisionD12.options.map((o) => `${o.id} ${o.label}`).join(' · ')
const recommended = decisionD12.options.find((o) => o.id === decisionD12.recommendation)

/** The fixture world's open attention items, each tied to the mission it blocks. */
export const fixtureAttention: readonly Attention[] = [
  {
    id: decisionD12.ref,
    kind: 'attention',
    revision: 'a1',
    updated_at: decisionD12.raisedAt,
    attention_kind: 'agent-request',
    title: decisionD12.title,
    detail: decisionD12.question,
    because: `Options: ${optionsLine}. MissionsUi recommends ${decisionD12.recommendation}${recommended ? ` (${recommended.label}): ${recommended.tradeoff}` : ''}`,
    priority: 'high',
    state: 'open',
    requested_at: decisionD12.raisedAt,
    person_id: decisionD12.owner,
    source_id: decisionD12.raisedBy,
    requester_id: decisionD12.raisedBy,
    mission_id: decisionD12.mission,
    mission_run_id: 'mission-run/webfractal/foundation/3',
    step_run_id: stepId({ missionPath: 'webfractal/foundation', n: 3, path: decisionD12.step }),
    actions: ['work.done'],
  },
  {
    id: 'attention/01JDQ7RS9CAS',
    kind: 'attention',
    revision: 'a3',
    updated_at: minutesAgo(4),
    attention_kind: 'human-gate',
    title: 'Approve deleting verified CAS originals',
    detail:
      '248,112 blobs (1.1 TiB) re-hashed with zero manifest diffs; /srv/cas-staging is full (ENOSPC) until the originals go.',
    because:
      'Step delete-originals-batch-1 is gated on operator approval; CasJanitor errored copying batch 2.',
    priority: 'high',
    state: 'open',
    requested_at: minutesAgo(266),
    person_id: operator.ref,
    source_id: stepId({ missionPath: casPath, n: 1, path: 'delete-originals-batch-1' }),
    mission_id: casMigration.id,
    mission_run_id: `mission-run/${casPath}/1`,
    step_run_id: stepId({ missionPath: casPath, n: 1, path: 'delete-originals-batch-1' }),
    actions: ['work.done'],
  },
  {
    id: 'attention/01JDQ7R2HLTH',
    kind: 'attention',
    revision: 'a1',
    updated_at: minutesAgo(318),
    attention_kind: 'revision-approval',
    title: 'Approve platform/health/daily revision 4',
    detail: 'Adds a Darwin activation probe to invariants; raises its timeout 1h → 90m.',
    because: 'HealthProbe proposed a revision via mission.revise after run 272 timed out.',
    priority: 'normal',
    state: 'open',
    requested_at: minutesAgo(318),
    person_id: operator.ref,
    source_id: dailyHealth.id,
    requester_id: healthProbe,
    mission_id: dailyHealth.id,
    actions: ['mission.approve-revision', 'mission.cancel-revision'],
  },
  {
    id: 'attention/01JDQ7RQ1SDK',
    kind: 'attention',
    revision: 'a1',
    updated_at: minutesAgo(47),
    attention_kind: 'fault',
    title: 'build-host-c went offline mid-run',
    detail:
      'SdkInterop was on step darwin (11 of 18 cases passed) when the heartbeat was lost 47m ago.',
    because: 'The run cannot continue until the host is back or the step is reassigned.',
    priority: 'normal',
    state: 'open',
    requested_at: minutesAgo(47),
    person_id: operator.ref,
    source_id: sdkInteropSeat,
    mission_id: sdkInterop.id,
    mission_run_id: 'mission-run/gateway/sdk-interop/2',
    step_run_id: stepId({ missionPath: 'gateway/sdk-interop', n: 2, path: 'darwin' }),
    actions: ['work.done'],
  },
  {
    id: 'attention/01JDQ7QK8RLM',
    kind: 'attention',
    revision: 'a1',
    updated_at: minutesAgo(180),
    attention_kind: 'launch-approval',
    title: 'Launch gateway/rate-limits?',
    detail: 'Three steps; measures live follows per client before enforcing a budget.',
    because: 'Proposed missions start only after operator approval.',
    priority: 'low',
    state: 'open',
    requested_at: minutesAgo(180),
    person_id: operator.ref,
    source_id: rateLimits.id,
    mission_id: rateLimits.id,
    actions: ['launch.approve', 'launch.cancel'],
  },
  {
    id: 'attention/01JDQ7RVDOCS',
    kind: 'attention',
    revision: 'a1',
    updated_at: minutesAgo(15),
    attention_kind: 'agent-request',
    title: 'Is build-host-c an on-call builder?',
    detail:
      'acme/docs#57 documents build-host-c as a laptop that may go offline; confirm before the runbook ships.',
    because: 'DocsWriter cannot verify host roles from the repositories.',
    priority: 'normal',
    state: 'open',
    requested_at: minutesAgo(15),
    person_id: operator.ref,
    source_id: docsWriter,
    requester_id: docsWriter,
    mission_id: runbooksRefresh.id,
    mission_run_id: 'mission-run/docs/runbooks-refresh/1',
    step_run_id: stepId({ missionPath: 'docs/runbooks-refresh', n: 1, path: 'confirm-topology' }),
    actions: ['work.done'],
  },
].map((attention) => decodeUnknownSync(Attention, 'strict')(attention))

// --- not-in-protocol extras ---------------------------------------------------------------------

/** Not-in-protocol fields per mission id: schedules, escalations, fenced actions and executors. */
export const fixtureProposed: Readonly<Record<string, ProposedMissionFields>> = {
  [webfractalFoundation.id]: {
    executor: workbenchShell,
    actions: [
      { id: 'mission.revise', fence: webfractalFoundation.mission_revision },
      { id: 'mission.cancel', fence: webfractalFoundation.mission_revision },
    ],
  },
  [weeklyCycle.id]: {
    executor: depsSteward,
    schedule: {
      name: 'cycle',
      every: '7d',
      anchor: onDay({ days: 3 + 7 * 39, time: '07:00' }),
      catchUp: 'latest',
      nextRunAt: onDay({ days: -4, time: '07:00' }),
      worksMissionId: weeklyCycle.id,
    },
    actions: [
      { id: 'mission.cancel', fence: weeklyCycle.mission_revision },
      { id: 'mission.revise', fence: weeklyCycle.mission_revision },
    ],
  },
  [dailyHealth.id]: {
    executor: healthProbe,
    schedule: {
      name: 'health',
      every: '1d',
      anchor: onDay({ days: 273, time: '06:00' }),
      catchUp: 'latest',
      nextRunAt: onDay({ days: -1, time: '06:00' }),
      worksMissionId: dailyHealth.id,
    },
    actions: [
      { id: 'mission.approve-revision', fence: dailyHealth.mission_revision },
      { id: 'mission.cancel-revision', fence: dailyHealth.mission_revision },
      { id: 'mission.cancel', fence: dailyHealth.mission_revision },
    ],
  },
  [casMigration.id]: {
    executor: casJanitor,
    actions: [{ id: 'mission.cancel', fence: casMigration.mission_revision }],
  },
  [rateLimits.id]: {
    actions: [{ id: 'mission.start', fence: rateLimits.mission_revision }],
  },
  [otelCutover.id]: {
    actions: [{ id: 'mission.revise', fence: otelCutover.mission_revision }],
  },
}

/** The world's 15 missions, in world order. */
export const fixtureMissions: readonly Mission[] = [
  webfractalFoundation,
  commandPalette,
  terminalRendererBakeoff,
  vistaEmbed,
  gatewaySchemaV1,
  sdkInterop,
  rateLimits,
  weeklyCycle,
  dailyHealth,
  casMigration,
  flakeTriage,
  secretsRotation,
  otelCutover,
  diskPressure,
  runbooksRefresh,
]

if (fixtureMissions.map((m) => m.id).join() !== worldMissions.map((m) => m.ref).join())
  throw new Error('missions fixtures: fixtureMissions must project every world mission in order')

/**
 * The `missions` collection window serves `run_details` with steps only for open runs and the
 * latest run; a detail read adds every run's steps (collections.md "Rows are joined in st").
 */
export const windowRow = (detail: Mission): Mission => {
  const runs = detail.run_details ?? []
  return {
    ...detail,
    run_details: runs.map((r, i) =>
      i === runs.length - 1 || !['completed', 'failed', 'cancelled'].includes(r.status)
        ? r
        : Object.assign({}, r, { steps: Option.none() }),
    ),
  }
}
