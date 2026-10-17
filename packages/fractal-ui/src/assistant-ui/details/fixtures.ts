import { fixtureSidebarNow, fixtureSidebarRows, fixtureMissionDTOs, fixtureAttentionDTOs } from './sidebar-fixtures'
import type { SidebarAgentRow } from '../sidebar/model'
import { files, resources } from './resource-fixtures'
import type { ResourceData as TasteResource, ResourceFile as TasteFile } from '../resources/resource-model'
import { detailsSectionOrder, type DetailsOverview, type DetailsResource, type DetailsSection, type DetailsSectionName, type DetailsState, type DetailsValue } from './model'
/**
 * Fixture provenance: shapes and counts are modeled on one real `st --json agents`
 * observation (2026-10-08T07:58:26Z, sanitized machine `workstation-a`): working
 * agent, harness omp, status-since 06:49:52, incarnation started the previous day
 * 22:35:26 (9h23m age), context 61,206/1,000,000 with 6 compactions, cumulative
 * tokens 480,035,214, cost unreported (the provider does not report cost).
 * The `busy` list perturbs counts of the observed resource kinds deterministically
 * (3 PRs, 5 files, 2 terminals, 2 subagents); identities stay the sanitized public
 * kit ones. Metrics arrive separately through TraceSeries; the overview carries
 * identity and timing observations only.
 */
export interface DetailsFixture {
  readonly state: DetailsState
  readonly overview: DetailsOverview
  readonly sections: readonly DetailsSection[]
}
const unknown: DetailsValue<never> = { _tag: 'Unknown' }
const known = <A,>(value: A): DetailsValue<A> => ({ _tag: 'Known', value })
const minute = 60_000, hour = 60 * minute, day = 24 * hour
/** Duration strings match the sidebar's coarse scale; every part stays behind its unit. */
export const detailsDuration = (ms: number): string => {
  if (ms < 0) return detailsDuration(0)
  if (ms < minute) return `${Math.floor(ms / 1000)}s`
  if (ms < hour) return `${Math.floor(ms / minute)}m`
  if (ms < day) return `${Math.floor(ms / hour)}h ${String(Math.floor((ms % hour) / minute)).padStart(2, '0')}m`
  return `${Math.floor(ms / day)}d ${String(Math.floor((ms % day) / hour)).padStart(2, '0')}h`
}
const root = fixtureSidebarRows.get('agent/sample')!
const idleRoot = fixtureSidebarRows.get('agent/source-archive')!
const emptyItems: readonly DetailsResource[] = []
const item = (uri: string, label: string, detail: string, glyph: string, card: TasteResource = { kind: 'file', label, detail, fileCount: unknown, files: [] }): DetailsResource => ({ uri, glyph, card })
const busyItems: Readonly<Record<DetailsSectionName, DetailsValue<readonly DetailsResource[]>>> = {
  Workspace: known([
    item('worktree:sample', resources[2]!.label, resources[2]!.detail, '▱', resources[2]),
    item('branch:sample', root.branch ?? 'Sample branch', 'Declared branch', '⑂'),
    item('repository:sample', 'sample-app', 'Repository', '▣'),
  ]),
  'Pull requests': known([
    item('pr:sample/42', resources[1]!.label, resources[1]!.detail, '⑂', resources[1]),
    item('pr:sample/43', 'PR #43 · Expand observation coverage', 'Open · sample-app · checks passed', '⑂'),
    item('pr:sample/44', 'PR #44 · Tighten projection fixtures', 'Draft · sample-app · checks running', '⑂'),
  ]),
  'CI runs': known([
    item('ci-run:sample/42', 'Projection checks', 'CI run · succeeded', '✓'),
    item('ci-run:sample/43', 'Observation coverage', 'CI run · running', '✓'),
  ]),
  Reviews: unknown,
  'Files touched': known(files.concat([
    { path: 'src/store.ts', added: known(3), removed: unknown, lines: unknown },
    { path: 'src/model.ts', added: known(12), removed: known(4), lines: unknown },
  ] as readonly TasteFile[]).map(file => ({ ...item(`diff:sample/${file.path}`, file.path, 'Modified file', '±', { kind: 'file', label: file.path, detail: 'Modified file', fileCount: known(1), files: [file] }), file }))),
  Terminals: known([
    { ...item('terminal:sample/1', 'Session terminal', 'Live output', '>_'), terminal: ['$ pnpm test', '✓ 2 passed · 184 ms'] },
    { ...item('terminal:sample/2', 'Build terminal', 'Idle', '>_'), terminal: ['$ pnpm build', 'done in 4.2s'] },
  ]),
  Subagents: known(root.children.flatMap(agent => [{ ...item(`agent:${agent.id}`, agent.title, agent.statusLabel, '●'), agent }])),
  'Missions / queue': known(fixtureMissionDTOs.slice(0, 3).map(mission => item(`mission:${mission.id.split('/').at(-1)}`, mission.title, String(mission.state), '◇'))),
  Attention: known(fixtureAttentionDTOs.filter(attention => attention.source_id === root.ref).map(attention => item(`attention:${attention.id.split('/').at(-1)}`, attention.title, attention.detail, '!'))),
  'Declared bindings': unknown,
  History: unknown,
}
/** The one observed machine identity is sanitized; every field maps to a snapshot fact. */
const busyOverview: DetailsOverview = {
  title: 'UI redesign milestone', status: 'working', activity: 'working', machine: 'workstation-a', harness: 'omp', model: 'reported-model-a',
  mission: 'UI redesign milestone',
  since: '1h 08m', age: '9h 23m',
}
/** Identity facts survive a sync gap; absent status and timing remain omitted. */
const pendingOverview: DetailsOverview = {
  title: 'UI redesign milestone', status: undefined, machine: 'workstation-a', harness: 'omp', model: 'reported-model-a',
  since: undefined, age: undefined,
}
function rowOverview(row: SidebarAgentRow): DetailsOverview {
  return {
    title: row.title, status: row.status, activity: row.status === 'working' || row.status === 'waiting' || row.status === 'idle' ? row.status : undefined, machine: row.host, harness: row.harness, model: row.model,
    since: row.statusSince === undefined ? undefined : detailsDuration(fixtureSidebarNow - row.statusSince),
    age: undefined,
  }
}
function createFixture(state: DetailsState): DetailsFixture {
  const overview = state === 'busy' || state === 'partial' || state === 'absent' ? busyOverview
    : state === 'new' ? { ...busyOverview, title: 'New workspace', mission: undefined, status: 'pending', activity: undefined, model: undefined, since: '0s', age: '0s' }
    : state === 'idle' ? rowOverview(idleRoot)
    : state === 'loading' || state === 'error' ? pendingOverview
    : pendingOverview
  return {
    state, overview,
    sections: detailsSectionOrder.map(title => ({
      title,
      items: state === 'new' ? known(emptyItems)
        : state === 'busy' || state === 'partial' || state === 'absent' ? busyItems[title]
        : title === 'Workspace' || title === 'Files touched' ? busyItems[title]
        : title === 'History' || title === 'Declared bindings' || title === 'Reviews' ? unknown
        : known(emptyItems),
    })),
  }
}
const fixtures: Readonly<Record<DetailsState, DetailsFixture>> = {
  busy: createFixture('busy'), idle: createFixture('idle'), new: createFixture('new'),
  partial: createFixture('partial'), unknown: createFixture('unknown'),
  absent: createFixture('absent'), loading: createFixture('loading'), error: createFixture('error'),
}
export const makeDetailsFixture = (state: DetailsState): DetailsFixture => fixtures[state]
