import * as Native from '@smalltalk/st3-client/schema'
import { DateTime, Option } from 'effect'
import { describe, expect, it } from 'vitest'
import { createProjections } from './projections.ts'
import { observed, unavailable, type Feed } from './source.ts'
import { nativeDetailsRoot, nativeDetailsTiming, projectNativeDetails } from './nativeDetails.ts'
import { projectNativeDeclarations, type NativeDeclarations } from './nativeDeclarations.ts'
import { contextOccupancy } from './contextOccupancy.ts'

const row = Native.decodeUnknownSync(Native.Agent)({ id: 'agent/example', kind: 'agent', name: 'Real seat', state: 'running', reachability: 'local', runtime_ids: [], revision: '1', updated_at: '1970-01-01T00:00:05Z', incarnation_id: 'inc/current', current_session_id: 'session/current', driver: 'omp', workspace: '/private/checkout', checkout: { repository: '/private/repo', branch: 'private-branch', base: 'main' },
  usage: { aggregation: 'cumulative-per-incarnation-else-response-deltas', incarnation_count: 1, input_tokens: 500, output_tokens: 500, cached_tokens: 0, total_tokens: 99999999, context: { used_tokens: 61206, window_tokens: 1000000, used_percent: 6.1206, observed_at_unix_ms: 4000, model: 'actual-model', compactions: 2, last_compaction_ms: 3000 } },
  todo: { accepted_at: '1970-01-01T00:00:04Z', claim_id: 'claim/todo', stale: false, snapshot: { harness: 'omp', incarnation_id: 'inc/current', session_id: 'native/root', observed_at: '1970-01-01T00:00:04Z', source_op: 'todo/write', phases: [], totals: { blocked: 0, completed: 0, in_progress: 0, pending: 0 }, truncated: false } },
})
const usageRow = { agent: row.id, native_session_id: 'native/root', model: 'first-model', input_tokens: 10, output_tokens: 20, cached_tokens: 3, cache_write_tokens: 4, cache_write_1h_tokens: 0, total_tokens: 37, cost_microusd: 1200000, reported_cost_microusd: 0, unpriced_tokens: 0 }
const period = Native.decodeUnknownSync(Native.UsagePeriod)({ since_ms: 1000, until_ms: 5000, rows: [usageRow, { ...usageRow, model: 'second-model', account: 'second-account', total_tokens: 63, cost_microusd: 2300000 }, { ...usageRow, agent: 'agent/other', cost_microusd: 99000000 }] })
const session = Native.decodeUnknownSync(Native.Session)({ id: 'session/current', kind: 'session', owner_id: row.id, runtime_incarnation: 'inc/current', started_at: '1970-01-01T00:00:01Z', ended_at: '1970-01-01T00:00:03Z', updated_at: '1970-01-01T00:00:03Z', revision: '1', state: 'completed', timeline_cursor: 'cursor/0' })
const missing = unavailable({ reason: 'unsupported', detail: 'Not reported' })
const project = ({ selected = row, selectedPeriod = observed({ value: period }), selectedSession = session, sessionFeed, selectedQueue, selectedDeclarations = missing, needsMe }: { readonly selected?: Native.Agent; readonly selectedPeriod?: Feed<Native.UsagePeriod>; readonly selectedSession?: Native.Session; readonly sessionFeed?: Feed<Native.Session>; readonly selectedQueue?: Feed<Native.AgentQueue>; readonly selectedDeclarations?: Feed<NativeDeclarations>; readonly needsMe?: boolean } = {}) => projectNativeDetails({ agentRef: row.id, row: observed({ value: selected }), agent: observed({ value: createProjections().fleetFromAgents([selected]).agents[0] }), period: selectedPeriod, session: sessionFeed ?? observed({ value: selectedSession }), declarations: selectedDeclarations, missions: observed({ value: [] }), queue: selectedQueue, needsMe, now: 10000, onRetry: () => undefined })

describe('fixture-free native Details projection', () => {
  it('keeps reported activity independent of missing period rows and stale observations', () => {
    const idle = { ...row, harness_state: Option.some('idle') }
    const working = { ...row, harness_state: Option.some('working') }
    for (const selectedPeriod of [
      missing,
      observed({ value: { ...period, rows: [] } }),
      { ...observed({ value: period }), freshness: 'stale' as const },
    ]) {
      expect(project({ selected: idle, selectedPeriod })).toMatchObject({ state: 'partial', overview: { activity: 'idle' } })
      expect(project({ selected: working, selectedPeriod })).toMatchObject({ state: 'partial', overview: { activity: 'working' } })
    }
    const unreported = projectNativeDetails({ agentRef: row.id, row: observed({ value: row }), agent: observed({ value: undefined }), period: missing, session: missing, declarations: missing, missions: missing, now: 10000, onRetry: () => undefined })
    expect(unreported.state).toBe('partial')
    expect(unreported.overview.activity).toBeUndefined()
  })
  it('sums attributed main cost across models/accounts with exact bounds, independently of cumulative tokens', () => {
    const result = project()
    expect(result.nativeSessionId).toBe('native/root')
    expect(result.trace.meters.spend.mainUsd).toMatchObject({ _tag: 'Known', value: 3.5 })
    expect(result.trace.meters.spend.tokens).toMatchObject({ _tag: 'Known', value: 99_999_999 })
    expect(result.trace.meters.spend.mainUsdWindow).toMatchObject({ _tag: 'Known', value: { _tag: 'Interval', fromMs: 1000, toMs: 5000 } })
    expect(result.trace.meters.spend.tokensScope).toEqual({ _tag: 'AgentIncarnations' })
    expect(result.trace.meters.spend.tokensWindow).toMatchObject({ _tag: 'Unknown', detail: 'Agent cumulative token accumulation start/window is not reported.' })
    expect(result.trace.meters.spend.totalUsd._tag).toBe('Unknown')
    expect(result.trace.meters.spend.mainUsdCoverage).toEqual({ _tag: 'Unavailable', reason: 'not-attributed' })
    expect(result.syncStatus).toBeUndefined()
  })
  it('keeps observed agent cumulative tokens when no current native root is proven, without inventing a window', () => {
    const result = project({ selected: { ...row, todo: Option.none() }, selectedPeriod: missing })
    expect(result.nativeSessionId).toBeUndefined()
    expect(result.trace.meters.spend.mainUsd._tag).toBe('Unknown')
    expect(result.trace.meters.spend.tokens).toEqual({ _tag: 'Known', value: 99_999_999, source: 'st' })
    expect(result.trace.meters.spend.tokensScope).toEqual({ _tag: 'AgentIncarnations' })
    expect(result.trace.meters.spend.tokensWindow._tag).toBe('Unknown')
    expect(project({ selected: { ...row, usage: Option.none() } }).trace.meters.spend.tokens._tag).toBe('Unknown')
  })
  it('exports canonical native owner identity separately from its display label', () => {
    const selected = { ...row, host_id: Option.some(Native.decodeUnknownSync(Native.HostId)('host/example')) }
    const result = project({ selected })
    expect(result.overview.machine).toBe('example')
    expect(result.host).toBe('host/example')
    expect(project().host).toBeUndefined()
  })
  it('does not relabel a work-step title as an unobserved mission title', () => {
    const work = Native.decodeUnknownSync(Native.WorkLabel)({ id: 'work/example', mission_id: 'mission/example', mission_run_id: 'mission-run/example', path: 'execute', state: 'claimed', since: '1970-01-01T00:00:01Z', title: 'Observed step title' })
    const result = project({ selected: { ...row, current_work: [work] } })
    expect(result.overview.mission).toBe('mission/example · mission title unreported · current work: Observed step title')
  })
  it('uses known sidebar attention, runtime terminal identity and the same native agent queue, not placeholder sections', () => {
    const selected = { ...row, runtime_ids: [Native.decodeUnknownSync(Native.RuntimeId)('runtime/example')], ask: Option.some('question'), blocked_on: Option.some('human'), reason: Option.some('Observed pending question') }
    const queue = Native.decodeUnknownSync(Native.AgentQueue)({ kind: 'agent-queue', agent_id: row.id, current_work_ids: ['work/example'], next_work_id: null, move_count: 0, moves: [], runs: [{ mission_run_id: 'mission-run/example', joined_at: '1970-01-01T00:00:01Z', position: 1, state: 'ready', run_state: 'active', claimed_work_ids: [], ready_work_ids: ['work/example'], waiting_work_ids: [] }] })
    const sections = project({ selected, selectedQueue: observed({ value: queue }), needsMe: true }).sections
    expect(JSON.stringify(sections.find(section => section.title === 'Attention'))).toContain('Needs your attention')
    expect(JSON.stringify(sections.find(section => section.title === 'Attention'))).toContain('Native ask · question')
    const terminals = sections.find(section => section.title === 'Terminals')?.items
    expect(terminals?._tag === 'Known' ? terminals.value.map(item => item.uri) : []).toEqual(['terminal/example'])
    expect(JSON.stringify(sections.find(section => section.title === 'Missions / queue'))).toContain('1 ready, 0 waiting')
    expect(project({ selectedQueue: observed({ value: { ...queue, agent_id: Native.decodeUnknownSync(Native.Id)('agent/other') } }) }).sections.find(section => section.title === 'Missions / queue')?.items._tag).toBe('Unknown')
    expect(project({ selectedQueue: { _tag: 'Observed', freshness: 'live', value: queue, error: { reason: 'ungranted', detail: 'Revoked' } } }).sections.find(section => section.title === 'Missions / queue')?.items._tag).toBe('Unknown')
    expect(project({ needsMe: false }).sections.find(section => section.title === 'Attention')?.items._tag).toBe('Known')
    expect(project().sections.find(section => section.title === 'Attention')?.items._tag).toBe('Unknown')
  })
  it('selects only the proven current binding, ignores unrelated old roots, and never selects from period rows alone', () => {
    expect(nativeDetailsRoot({ row: { ...row, todo: Option.none() }, period })).toBeUndefined()
    expect(nativeDetailsRoot({ row: { ...row, incarnation_id: Option.some('inc/new') }, period })).toBeUndefined()
    const oldTodo = Option.getOrThrow(row.todo)
    expect(nativeDetailsRoot({ row: { ...row, todo: Option.some({ ...oldTodo, stale: true }) }, period })).toBeUndefined()
    expect(nativeDetailsRoot({ row: { ...row, driver: Option.some('claude') }, period })).toBeUndefined()
    expect(nativeDetailsRoot({ row, period: { ...period, rows: [] } })).toBeUndefined()
    expect(nativeDetailsRoot({ row, period: { ...period, rows: [{ ...period.rows[0]!, native_session_id: 'native/old' }] } })).toBeUndefined()
    // An older incarnation's root in the same window is unrelated history and is never summed.
    const withOldRoot = { ...period, rows: [...period.rows, { ...period.rows[0]!, native_session_id: 'native/old', cost_microusd: 7000000, total_tokens: 900 }] }
    expect(nativeDetailsRoot({ row, period: withOldRoot })).toBe('native/root')
    const result = project({ selectedPeriod: observed({ value: withOldRoot }) })
    expect(result.trace.meters.spend.mainUsd).toMatchObject({ _tag: 'Known', value: 3.5 })
    expect(result.trace.meters.spend.tokens).toMatchObject({ _tag: 'Known', value: 99_999_999 })
    // Conflicting current-incarnation bindings are ambiguous.
    const suspended = Native.decodeUnknownSync(Native.AgentSuspension)({ action: 'resume', blocking: [], incarnation_id: 'inc/current', native_session_id: 'native/other', operation_id: 'operation/resume', phase: 'resumed', updated_at: '1970-01-01T00:00:04Z' })
    expect(nativeDetailsRoot({ row: { ...row, suspension: Option.some(suspended) }, period: withOldRoot })).toBeUndefined()
    expect(nativeDetailsRoot({ row: { ...row, todo: Option.none(), suspension: Option.some({ ...suspended, native_session_id: Option.some('native/old') }) }, period: withOldRoot })).toBe('native/old')
    expect(nativeDetailsRoot({ row: { ...row, todo: Option.none(), suspension: Option.some({ ...suspended, native_session_id: Option.some('native/old'), phase: 'restoring' }) }, period: withOldRoot })).toBeUndefined()
  })
  it('preserves zero priced spend, distinguishes no rows, and labels unpriced coverage', () => {
    const zero = { ...period, rows: [{ ...period.rows[0]!, cost_microusd: 0 }] }
    expect(project({ selectedPeriod: observed({ value: zero }) }).trace.meters.spend.mainUsd).toMatchObject({ _tag: 'Known', value: 0 })
    expect(project({ selectedPeriod: observed({ value: { ...period, rows: [] } }) }).trace.meters.spend.mainUsd._tag).toBe('Unknown')
    expect(project({ selectedPeriod: observed({ value: { ...zero, rows: [{ ...zero.rows[0]!, unpriced_tokens: 5 }] } }) }).trace.meters.spend.mainUsdCoverage).toEqual({ _tag: 'Unavailable', reason: 'not-covered' })
  })
  it('maps selected native refusal to non-retryable sync causes and preserves real codes', () => {
    const refused = (feed: Feed<never>) => projectNativeDetails({ agentRef: row.id, row: feed, agent: feed, period: missing, session: missing, declarations: missing, missions: missing, now: 10000, onRetry: () => undefined }).syncStatus
    expect(refused(unavailable({ reason: 'ungranted', detail: 'read.projections denied' }))).toEqual({ _tag: 'Failed', cause: { _tag: 'Server', code: 'forbidden', message: 'read.projections denied' } })
    expect(refused(unavailable({ reason: 'unsupported', detail: 'No agents family' }))).toEqual({ _tag: 'Failed', cause: { _tag: 'Server', code: 'unsupported', message: 'No agents family' } })
    expect(refused(unavailable({ reason: 'failed', detail: 'Gateway restarted', code: 'internal' }))).toEqual({ _tag: 'Failed', cause: { _tag: 'Server', code: 'internal', message: 'Gateway restarted' } })
    expect(refused(unavailable({ reason: 'failed', detail: 'Lost' }))).toEqual({ _tag: 'Failed', cause: { _tag: 'Unknown' } })
  })
  it('retains stale period values without exporting a stale native root to supplements', () => {
    const result = project({ selectedPeriod: { _tag: 'Observed', value: period, freshness: 'stale', error: { reason: 'failed', detail: 'Read failed' } } })
    expect(result.stale).toBe(true)
    expect(result.nativeSessionId).toBeUndefined()
    expect(result.trace.nativeSessionId).toEqual({ _tag: 'Known', value: 'native/root', source: 'st' })
    expect(result.trace.meters.spend.mainUsd).toMatchObject({ _tag: 'Known', value: 3.5 })
  })
  it('refuses producer snapshot-dated starts even seven seconds later, with the actual reason only on unknown values', () => {
    const snapshotCreatedAt = DateTime.makeUnsafe(3000)
    const snapshotSession = { ...session, started_at: snapshotCreatedAt, updated_at: DateTime.makeUnsafe(5000), ended_at: Option.none(), state: 'running' as const }
    expect(nativeDetailsTiming({ row, session: snapshotSession, snapshotCreatedAt, now: 10000 })).toEqual({ detail: 'Session start matches the producer snapshot time; elapsed time and age are unreported.' })
    expect(nativeDetailsTiming({ row, session: { ...session, started_at: session.updated_at }, now: 10000 })).toEqual({ detail: 'Session start matches its update time; elapsed time and age are unreported.' })
    const rejected = project({ sessionFeed: observed({ value: snapshotSession, snapshotCreatedAt }) })
    expect(rejected.overview.durations?.map(item => item.value)).toEqual(['Unreported', 'Unreported'])
    expect(rejected.overview.durations?.every(item => item.detail?.includes('producer snapshot time'))).toBe(true)
    expect(rejected.nativeSessionId).toBe('native/root')
    expect(rejected.trace.meters.spend.mainUsd).toMatchObject({ _tag: 'Known', value: 3.5 })
    const valid = project({ sessionFeed: observed({ value: session, snapshotCreatedAt }) })
    expect(valid.overview.durations?.map(item => item.value)).toEqual(['2s', '9s'])
    expect(valid.overview.durations?.every(item => item.detail === undefined)).toBe(true)
    // Client receipt/display time is not snapshot provenance: an actual start now is valid zero.
    expect(nativeDetailsTiming({ row, session: { ...snapshotSession, started_at: DateTime.makeUnsafe(10000) }, snapshotCreatedAt, now: 10000 })).toEqual({ elapsedMs: 0, sessionAgeMs: 0 })
    const guarded = project({ sessionFeed: unavailable({ reason: 'failed', detail: 'Session start changed between observations; elapsed time and age are unreported.' }) })
    expect(guarded.overview.durations?.every(item => item.detail === 'Session start changed between observations; elapsed time and age are unreported.')).toBe(true)
  })
  it('marks only actively waiting declarations and queue as pending, never static missing producers', () => {
    const waiting = project({ selectedDeclarations: { _tag: 'Waiting' }, selectedQueue: { _tag: 'Waiting' } })
    expect(waiting.sections.find(section => section.title === 'Declared bindings')?.pending).toBe(true)
    expect(waiting.sections.find(section => section.title === 'Missions / queue')?.pending).toBe(true)
    const absent = project()
    expect(absent.sections.find(section => section.title === 'Declared bindings')?.pending).toBe(false)
    expect(absent.sections.find(section => section.title === 'Missions / queue')?.pending).not.toBe(true)
    expect(absent.sections.find(section => section.title === 'History')?.pending).not.toBe(true)
  })
  it('carries first native root acquisition instead of declaring missing root prerequisites', () => {
    const pendingPeriod = project({ selectedPeriod: { _tag: 'Waiting' } })
    expect(pendingPeriod.nativeRootPending).toBe(true)
    expect(pendingPeriod.nativeSessionId).toBeUndefined()
    expect(project().nativeRootPending).toBe(false)
    expect(project({ selectedPeriod: missing }).nativeRootPending).toBe(false)
    const pendingRow = projectNativeDetails({ agentRef: row.id, agent: { _tag: 'Waiting' }, row: { _tag: 'Waiting' }, session: { _tag: 'Waiting' }, period: { _tag: 'Waiting' }, declarations: missing, missions: missing, now: 10000, onRetry: () => {} })
    expect(pendingRow.nativeRootPending).toBe(true)
    expect(pendingRow.sections.find(section => section.title === 'Declared bindings')?.pending).toBe(true)
  })
  it('shows managed elapsed and age pending until the session first read settles', () => {
    const pending = project({ sessionFeed: { _tag: 'Waiting' } })
    expect(pending.overview.durations?.map(item => ({ value: item.value, pending: item.pending }))).toEqual([
      { value: 'Loading', pending: true }, { value: 'Loading', pending: true },
    ])
    expect(pending.overview.title).toBe('Real seat')
    expect(project({ sessionFeed: missing }).overview.durations?.map(item => ({ value: item.value, pending: item.pending }))).toEqual([
      { value: 'Unreported', pending: false }, { value: 'Unreported', pending: false },
    ])
    expect(project().overview.durations?.every(item => item.pending === false)).toBe(true)
  })
  it('uses reported occupancy rather than cumulative totals, preserving main sidebar scope', () => {
    const agent = createProjections().fleetFromAgents([row]).agents[0]!
    const occupancy = contextOccupancy({ usage: agent.usage, now: 10000, observedAfter: agent.contextObservedAfter })
    const details = project()
    expect(occupancy._tag).toBe('Known')
    if (occupancy._tag === 'Known') expect(occupancy.usedPercent).toBeCloseTo(6.1206)
    expect(details.trace.meters.context.reportedPercent).toMatchObject({ _tag: 'Known', asOfMs: 4000 })
    if (details.trace.meters.context.reportedPercent._tag === 'Known') expect(details.trace.meters.context.reportedPercent.value).toBeCloseTo(6.1206)
    expect(details.trace.meters.context.compactions).toMatchObject({ _tag: 'Known', value: 2 })
    expect(details.trace.meters.context.lastCompactionMs).toMatchObject({ _tag: 'Known', value: 3000 })
    const { context: _context, ...withoutContext } = Option.getOrThrow(row.usage)
    expect(project({ selected: { ...row, usage: Option.some(withoutContext) } }).trace.meters.context.reportedPercent._tag).toBe('Unknown')
  })
  it('fences occupancy across incarnation changes using the shared retained projection', () => {
    const projections = createProjections()
    projections.fleetFromAgents([row])
    const next = { ...row, incarnation_id: Option.some('inc/next') }
    const agent = projections.fleetFromAgents([next]).agents[0]
    const result = projectNativeDetails({ agentRef: row.id, row: observed({ value: next }), agent: observed({ value: agent }), period: observed({ value: period }), session: observed({ value: session }), declarations: missing, missions: observed({ value: [] }), now: 10000, onRetry: () => undefined })
    expect(result.trace.meters.context.reportedPercent._tag).toBe('Unknown')
    expect(result.trace.meters.context.compactions._tag).toBe('Unknown')
    expect(project().overview.model).toBe('actual-model')
    expect(result.overview.model).toBeUndefined()
  })
  it('keeps managed elapsed independent of session age, status-since and root identity', () => {
    expect(nativeDetailsTiming({ row, session, now: 10000 })).toEqual({ elapsedMs: 2000, sessionAgeMs: 9000 })
    expect(nativeDetailsTiming({ row, session, now: 20000 })).toEqual({ elapsedMs: 2000, sessionAgeMs: 19000 })
    expect(nativeDetailsTiming({ row, session: { ...session, state: 'running', ended_at: Option.none() }, now: 20000 })).toEqual({ elapsedMs: 19000, sessionAgeMs: 19000 })
    expect(nativeDetailsTiming({ row, session: { ...session, ended_at: Option.none() }, now: 20000 })).toEqual({ sessionAgeMs: 19000 })
    expect(nativeDetailsTiming({ row, session: { ...session, owner_id: 'agent/other' }, now: 10000 })).toEqual({})
    expect(nativeDetailsTiming({ row, session: { ...session, runtime_incarnation: Option.some('inc/old') }, now: 10000 })).toEqual({})
    const result = project({ selected: { ...row, todo: Option.none(), since: Option.some(DateTime.makeUnsafe(7000)) } })
    expect(result.nativeSessionId).toBeUndefined()
    expect(result.overview.durations?.[0]?.value).toBe('2s')
    expect(result.overview.since).toBe('3s')
    expect(result.overview.age).toBe('Incarnation launch age unreported')
    // Both values derive from Session.started_at; neither claims a native launch fact.
    expect(result.overview.durations?.map(item => item.label)).toEqual([
      'Managed-session elapsed (from session start to end or now; wall time, not active)',
      'Current-incarnation session age (from managed session start; native launch not reported)',
    ])
  })
  it('keeps declared/requested resources distinct and hides paths/ref targets', () => {
    const result = project()
    expect(result.sections.find(item => item.title === 'Workspace')?.items._tag).toBe('Known')
    expect(JSON.stringify(result)).not.toContain('/private/')
    expect(JSON.stringify(result)).not.toContain('private-branch')
    expect(result.sections.find(item => item.title === 'Subagents')?.items._tag).toBe('Unknown')
    expect(project({ selected: { ...row, subagents: [] } }).sections.find(item => item.title === 'Subagents')?.items).toEqual({ _tag: 'Known', value: [] })
    expect(result.trace.series._tag).toBe('Unknown')
    expect(result.trace.turns._tag).toBe('Unknown')
    expect(result.trace.history._tag).toBe('Unknown')
    const workspace = result.sections.find(item => item.title === 'Workspace')?.items
    expect(workspace?._tag === 'Known' ? workspace.value.map(item => item.uri) : []).toEqual(['details:workspace/declared', 'details:workspace/requested-checkout'])
  })
  it('shows declared model only under declared bindings, never as the runtime model', () => {
    const declared = { _tag: 'Observed' as const, freshness: 'live' as const, value: { agentRef: row.id, revision: 'declaration-one', declaredModel: 'declared-model', bindings: [{ kind: 'workspace', count: 1 }] } }
    const usage = Option.getOrThrow(row.usage)
    const { model: _model, ...contextWithoutModel } = usage.context!
    const selected = { ...row, usage: Option.some({ ...usage, context: contextWithoutModel }) }
    const result = projectNativeDetails({ agentRef: row.id, row: observed({ value: selected }), agent: observed({ value: createProjections().fleetFromAgents([selected]).agents[0] }), period: observed({ value: period }), session: observed({ value: session }), declarations: declared, missions: observed({ value: [] }), now: 10000, onRetry: () => undefined })
    expect(result.overview.model).toBeUndefined()
    const bindings = result.sections.find(item => item.title === 'Declared bindings')?.items
    expect(bindings?._tag === 'Known' ? bindings.value.map(item => [item.uri, item.card.label]) : []).toEqual([['details:binding/0', 'Declared workspace'], ['details:binding/model', 'Declared model']])
    const stale = projectNativeDetails({ agentRef: row.id, row: observed({ value: selected }), agent: observed({ value: undefined }), period: missing, session: missing, declarations: { ...declared, value: { ...declared.value, agentRef: 'agent/other' } }, missions: missing, now: 10000, onRetry: () => undefined })
    expect(stale.sections.find(item => item.title === 'Declared bindings')?.items._tag).toBe('Unknown')
  })
  it('validates normalized declarations without emitting env, argv, paths or raw KDL', () => {
    const declaration = Native.decodeUnknownSync(Native.AgentDeclaration)({ id: row.id, revision: '1', revisions: ['1'], kdl: 'never emitted', tree: { name: 'agent', arguments: ['private-seat'], properties: {}, children: [ { name: 'workspace', arguments: ['/private/workspace'] }, { name: 'env', properties: { SAMPLE: 'test' } }, { name: 'argv', arguments: ['/private/tool'] }, { name: 'harness', arguments: ['omp'], children: [{ name: 'model', arguments: ['declared-model'] }, { name: 'account-pool', arguments: ['private-pool'] }] } ] } })
    const projected = projectNativeDeclarations({ declaration })
    expect(projected).toEqual({ agentRef: row.id, revision: '1', declaredModel: 'declared-model', bindings: [{ kind: 'workspace', count: 1 }, { kind: 'harness account-pool', count: 1 }] })
    expect(projectNativeDeclarations({ declaration: { ...declaration, tree: { name: 'agent', children: [{ name: 'env', arguments: {} }] } } })).toBeUndefined()
    expect(projectNativeDeclarations({ declaration: { ...declaration, tree: { name: 'agent', children: [] } } })?.bindings).toEqual([])
  })
  it('schedules main duration text at seconds then minutes and wakes future observation fences', () => {
    const projectAt = (now: number) => projectNativeDetails({
      agentRef: row.id, row: observed({ value: row }), agent: observed({ value: createProjections().fleetFromAgents([row]).agents[0] }),
      period: observed({ value: period }), session: observed({ value: session }), declarations: missing, missions: observed({ value: [] }), now, onRetry: () => undefined,
    })
    expect(projectAt(1500).nextChangeAt).toBe(2000)
    expect(projectAt(10_000).nextChangeAt).toBe(11_000)
    expect(projectAt(121_500).nextChangeAt).toBe(181_000)
    // Main's hours-and-minutes string keeps minute precision even after a day.
    expect(projectAt(90_001_500).nextChangeAt).toBe(90_061_000)
  })
})
