import type * as Native from '@smalltalk/st3-client/schema'
import type { DetailsOverview, DetailsSection, DetailsState, DetailsResource, TraceSeries, TraceFact, TraceCoverage, TraceAccumulationWindow } from '@smalltalk/fractal-ui/assistant-ui'
import type { SyncStatus } from '@smalltalk/fractal-ui/assistant-ui/sync-status'
import { DateTime, Duration, Option } from 'effect'
import { earliestChange } from '@smalltalk/fractal-ui/human-time/format'
import { contextOccupancy } from './contextOccupancy.ts'
import type { Agent, Feed } from './source.ts'
import type { NativeDeclarations } from './nativeDeclarations.ts'

export interface NativeDetailsObservation {
  readonly overview: DetailsOverview
  readonly sections: readonly DetailsSection[]
  readonly trace: TraceSeries
  readonly state: DetailsState
  readonly observedAt?: number
  readonly stale: boolean
  readonly nativeSessionId?: string
  /** The native row or usage-period first read has not settled; absence is not yet known. */
  readonly nativeRootPending: boolean
  readonly incarnationId?: string
  /** Canonical `Agent.host_id` for identity checks, never the shortened display label. */
  readonly host?: string
  readonly harness?: string
  /** Selected native row refusal; forbidden/unsupported never offer a same-request retry. */
  readonly syncStatus?: SyncStatus
  readonly onRetry: () => void
  /** Next displayed duration step or future-observation fence; absent means no timed wake. */
  readonly nextChangeAt?: number
}
/** Read-only display keys; `details:` URIs have no native route and must not be opened. */
export const nativeDetailsUriPrefix = 'details:'
const rowSyncStatus = (feed: Feed<unknown>): SyncStatus | undefined => {
  if (feed._tag !== 'Unavailable') return undefined
  if (feed.reason === 'ungranted') return { _tag: 'Failed', cause: { _tag: 'Server', code: feed.code ?? 'forbidden', message: feed.detail } }
  if (feed.reason === 'unsupported') return { _tag: 'Failed', cause: { _tag: 'Server', code: feed.code ?? 'unsupported', message: feed.detail } }
  return { _tag: 'Failed', cause: feed.code === undefined ? { _tag: 'Unknown' } : { _tag: 'Server', code: feed.code, message: feed.detail } }
}
const unknown = <T>(detail: string, reason: 'not-covered' | 'not-attributed' | 'loading' | 'failed' = 'not-covered'): TraceFact<T> => ({ _tag: 'Unknown', reason, detail })
const known = <T>(value: T, asOfMs?: number): TraceFact<T> => ({ _tag: 'Known', value, source: 'st', ...(asOfMs === undefined ? {} : { asOfMs }) })

/** A period row lacks incarnation identity. Only an explicit current-incarnation binding can fence it. */
export const nativeDetailsRoot = ({ row, period }: { readonly row: Native.Agent; readonly period: Native.UsagePeriod }): string | undefined => {
  const incarnation = Option.getOrUndefined(row.incarnation_id)
  if (incarnation === undefined) return undefined
  const bindings = new Set<string>()
  const todo = Option.getOrUndefined(row.todo)
  if (todo !== undefined && !todo.stale && todo.snapshot.incarnation_id === incarnation &&
    todo.snapshot.harness === Option.getOrUndefined(row.driver) && todo.snapshot.session_id.trim().length > 0) bindings.add(todo.snapshot.session_id)
  const suspension = Option.getOrUndefined(row.suspension)
  const suspensionRoot = suspension === undefined ? undefined : Option.getOrUndefined(suspension.native_session_id)
  if (suspension !== undefined && Option.getOrUndefined(suspension.incarnation_id) === incarnation &&
    (suspension.phase === 'suspended' || suspension.phase === 'resumed') && suspensionRoot !== undefined && suspensionRoot.trim().length > 0) bindings.add(suspensionRoot)
  const [root] = bindings
  // Older roots for this agent are unrelated history, not ambiguity of the proven binding.
  return bindings.size === 1 && root !== undefined && period.rows.some(item => item.agent === row.id && item.native_session_id === root) ? root : undefined
}

/** Snapshot-dated replica starts are not evidence of a real session start. */
export const nativeSessionStartReason = ({ session, snapshotCreatedAt }: { readonly session: Native.Session; readonly snapshotCreatedAt?: Native.Timestamp }): string | undefined => {
  const start = DateTime.toEpochMillis(session.started_at)
  if (snapshotCreatedAt !== undefined && start === DateTime.toEpochMillis(snapshotCreatedAt))
    return 'Session start matches the producer snapshot time; elapsed time and age are unreported.'
  if (start === DateTime.toEpochMillis(session.updated_at))
    return 'Session start matches its update time; elapsed time and age are unreported.'
  return undefined
}

export const nativeDetailsTiming = ({ row, session, snapshotCreatedAt, now }: { readonly row: Native.Agent; readonly session: Native.Session | undefined; readonly snapshotCreatedAt?: Native.Timestamp; readonly now: number }): { readonly elapsedMs?: number; readonly sessionAgeMs?: number; readonly detail?: string } => {
  const incarnation = Option.getOrUndefined(row.incarnation_id)
  if (session === undefined || incarnation === undefined || session.id !== Option.getOrUndefined(row.current_session_id) ||
    session.owner_id !== row.id || Option.getOrUndefined(session.runtime_incarnation) !== incarnation) return {}
  const start = DateTime.toEpochMillis(session.started_at)
  const detail = nativeSessionStartReason({ session, snapshotCreatedAt })
  if (detail !== undefined) return { detail }
  const ended = Option.getOrUndefined(session.ended_at)
  const end = ended === undefined ? now : DateTime.toEpochMillis(ended)
  if (start > now || end < start || end > now) return {}
  // A terminal state without an end cannot be projected as still running.
  const terminal = session.state === 'completed' || session.state === 'failed' || session.state === 'cancelled'
  return { sessionAgeMs: now - start, ...(terminal && ended === undefined ? {} : { elapsedMs: end - start }) }
}
const durationText = (ms: number): string => {
  const seconds = Math.floor(ms / 1000)
  return seconds < 60 ? `${seconds}s` : seconds < 3600 ? `${Math.floor(seconds / 60)}m` : `${Math.floor(seconds / 3600)}h ${Math.floor(seconds % 3600 / 60)}m`
}
const elapsedChange = (at: number | undefined, now: number): number | undefined => {
  if (at === undefined) return undefined
  if (at > now) return at
  const unit = now - at < 60_000 ? 1000 : 60_000
  return at + (Math.floor((now - at) / unit) + 1) * unit
}
const factFailure = (feed: Feed<unknown>): string => feed._tag === 'Waiting' ? 'Loading native observation.'
  : feed._tag === 'Unavailable' ? feed.reason === 'ungranted' ? 'Native read is not granted.' : feed.reason === 'unsupported' ? 'Native observation is unsupported.' : 'Native observation failed.'
  : feed.error?.reason === 'ungranted' ? 'Native read is not granted.' : 'Not reported by the native source.'
const resource = ({ key, label, detail }: { readonly key: string; readonly label: string; readonly detail: string }): DetailsResource => ({ uri: `${nativeDetailsUriPrefix}${key}`, glyph: 'resource', card: { kind: 'worktree', label, detail, fileCount: { _tag: 'Unknown' }, files: [] } })

/** Pure host DTO boundary: no fixture fallback and no private producer dependencies. */
export const projectNativeDetails = ({ agentRef, agent, row: rowFeed, session: sessionFeed, period: periodFeed, declarations, missions, queue, needsMe, now, onRetry }: {
  readonly agentRef: string
  readonly agent: Feed<Agent | undefined>
  readonly row: Feed<Native.Agent | undefined>
  readonly session: Feed<Native.Session>
  readonly period: Feed<Native.UsagePeriod>
  readonly declarations: Feed<NativeDeclarations>
  readonly missions: Feed<readonly Native.Mission[]>
  readonly queue?: Feed<Native.AgentQueue>
  /** The same selected-agent attention union used by the sidebar, not inferred from runtime waiting. */
  readonly needsMe?: boolean
  readonly now: number
  readonly onRetry: () => void
}): NativeDetailsObservation => {
  const row = rowFeed._tag === 'Observed' && rowFeed.error?.reason !== 'ungranted' ? rowFeed.value : undefined
  const projected = agent._tag === 'Observed' && agent.error?.reason !== 'ungranted' ? agent.value : undefined
  const session = sessionFeed._tag === 'Observed' && sessionFeed.error?.reason !== 'ungranted' ? sessionFeed.value : undefined
  const period = periodFeed._tag === 'Observed' && periodFeed.error?.reason !== 'ungranted' ? periodFeed.value : undefined
  const declared = declarations._tag === 'Observed' && declarations.error?.reason !== 'ungranted' && declarations.value.agentRef === agentRef ? declarations.value : undefined
  const stale = [agent, rowFeed, sessionFeed, periodFeed, declarations, missions].some(feed => feed._tag === 'Observed' && feed.freshness === 'stale')
  const incarnationId = row === undefined ? undefined : Option.getOrUndefined(row.incarnation_id)
  const attributedRoot = row === undefined || period === undefined ? undefined : nativeDetailsRoot({ row, period })
  // Last-good period data may be displayed stale but cannot authorize a private root query.
  const root = row === undefined || period === undefined || rowFeed._tag !== 'Observed' || rowFeed.freshness !== 'live' || periodFeed._tag !== 'Observed' || periodFeed.freshness !== 'live'
    ? undefined : nativeDetailsRoot({ row, period })
  const occupancy = contextOccupancy({ usage: projected?.usage ?? { _tag: 'Unknown' }, now, ...(projected?.contextObservedAfter === undefined ? {} : { observedAfter: projected.contextObservedAfter }) })
  const cumulativeUsage = row === undefined ? undefined : Option.getOrUndefined(row.usage)
  const nativeHost = row === undefined ? undefined : Option.getOrUndefined(row.host_id)
  const context = cumulativeUsage?.context
  const contextAsOf = context === undefined ? undefined : DateTime.toEpochMillis(context.observed_at_unix_ms)
  const contextValid = contextAsOf !== undefined && contextAsOf <= now && (projected?.contextObservedAfter === undefined || contextAsOf > projected.contextObservedAfter)
  const contextField = (value: number | undefined): TraceFact<number> => value === undefined || !contextValid ? unknown('Latest reported seat context; current-incarnation provenance is not reported.') : known(value, contextAsOf)
  const rows = attributedRoot === undefined || period === undefined ? [] : period.rows.filter(item => item.agent === agentRef && item.native_session_id === attributedRoot)
  const nativeWindow: TraceFact<TraceAccumulationWindow> = period !== undefined && rows.length > 0 && period.since_ms < period.until_ms
    ? known({ _tag: 'Interval', fromMs: period.since_ms, toMs: period.until_ms }, period.until_ms)
    : unknown('No attributed native reported period.', periodFeed._tag === 'Waiting' ? 'loading' : 'not-attributed')
  const hasPeriod = rows.length > 0 && nativeWindow._tag === 'Known'
  // Unpriced tokens are not expressible in kit coverage; the amount keeps unavailable attribution coverage.
  const cost = rows.reduce((sum, item) => sum + item.cost_microusd, 0) / 1_000_000
  // API period groups expose latest native ID, not their incarnation or binding history.
  const nativeCoverage: TraceCoverage = { _tag: 'Unavailable', reason: rows.some(item => item.unpriced_tokens > 0) ? 'not-covered' : 'not-attributed' }
  const unsupported = unknown<number>('Native source does not report descendant-inclusive spend.')
  const timing = row === undefined ? {} : nativeDetailsTiming({ row, session, snapshotCreatedAt: sessionFeed._tag === 'Observed' ? sessionFeed.snapshotCreatedAt : undefined, now })
  const timingDetail = timing.detail ?? (sessionFeed._tag === 'Unavailable' ? sessionFeed.detail : undefined)
  const timingPending = rowFeed._tag === 'Waiting' || sessionFeed._tag === 'Waiting'
  const syncStatus = rowSyncStatus(rowFeed)
  const statusSince = row === undefined ? undefined : Option.getOrUndefined(row.since)
  const model = contextValid && context?.model !== undefined && context.model.trim().length > 0 && context.model !== '.unknown' ? context.model : undefined
  const work = row?.current_work?.[0]
  const missionTitle = work === undefined || missions._tag !== 'Observed' ? undefined : missions.value.find(item => item.id === work.mission_id)?.title
  const workTitle = work === undefined ? undefined : Option.getOrUndefined(work.title)
  const mission = work === undefined ? row === undefined ? undefined : 'No current mission reported'
    : missionTitle ?? `${work.mission_id} · mission title ${missions._tag === 'Observed' ? 'unreported' : 'unavailable'}${workTitle === undefined ? '' : ` · current work: ${workTitle}`}`
  const workspaceItems: DetailsResource[] = []
  if (row !== undefined && Option.isSome(row.workspace)) workspaceItems.push(resource({ key: 'workspace/declared', label: 'Declared workspace', detail: 'Configured on the owning host; path withheld. Observed checkout/cwd confirmation is not reported.' }))
  if (row !== undefined && Option.isSome(row.checkout)) workspaceItems.push(resource({ key: 'workspace/requested-checkout', label: 'Requested repository / branch', detail: 'Repository, base and branch are declared intent; reference targets withheld. Successful checkout is not observed.' }))
  const nativeQueue = queue?._tag === 'Observed' && queue.error?.reason !== 'ungranted' && queue.value.agent_id === agentRef ? queue.value : undefined
  const queuedResources = nativeQueue === undefined ? [] : [
    resource({ key: 'queue/summary', label: `Current work · ${nativeQueue.current_work_ids.length}`, detail: `${nativeQueue.runs.length} observed mission runs in this agent queue${queue?._tag === 'Observed' && queue.freshness === 'stale' ? ' · last observed queue' : ''}` }),
    ...nativeQueue.runs.map(run => resource({ key: `queue/${run.mission_run_id}`, label: `Mission run · ${run.mission_run_id}`, detail: `${run.state} · ${run.run_state} · ${run.ready_work_ids.length} ready, ${run.waiting_work_ids.length} waiting · position ${run.position}` })),
  ]
  const terminal = row !== undefined && row.runtime_ids.length > 0 ? projected?.terminal : undefined
  const sections: DetailsSection[] = [
    { title: 'Workspace', items: workspaceItems.length === 0 ? { _tag: 'Unknown' } : { _tag: 'Known', value: workspaceItems }, unavailableReason: 'Declared workspace/checkout not reported; observed workspace confirmation is unsupported.' },
    { title: 'Subagents', items: row?.subagents === undefined ? { _tag: 'Unknown' } : { _tag: 'Known', value: row.subagents.map((child, index) => resource({ key: `subagent/${index}`, label: `Observed st subagent ${index + 1}`, detail: `${child.driver} · ${Option.getOrUndefined(child.subagent_type) ?? 'type unreported'} · status unobserved${Option.match(child.started_at, { onNone: () => ' · start unreported', onSome: at => ` · started ${new Date(DateTime.toEpochMillis(at)).toISOString()}` })}` })) }, unavailableReason: 'Currently leased st subagents are not reported; not a traced-descendant list.' },
    { title: 'Declared bindings', items: declared === undefined ? { _tag: 'Unknown' } : { _tag: 'Known', value: [
      ...declared.bindings.map((binding, index) => resource({ key: `binding/${index}`, label: `Declared ${binding.kind}`, detail: `${binding.count} reference${binding.count === 1 ? '' : 's'} · revision ${declared.revision} · targets withheld; declared intent, not observed resources.` })),
      ...(declared.declaredModel === undefined ? [] : [resource({ key: 'binding/model', label: 'Declared model', detail: `${declared.declaredModel} · revision ${declared.revision} · declared harness setting, not the observed runtime model.` })]),
    ] }, pending: rowFeed._tag === 'Waiting' || declarations._tag === 'Waiting', unavailableReason: factFailure(declarations) },
    { title: 'Terminals', items: row === undefined ? { _tag: 'Unknown' } : { _tag: 'Known', value: terminal === undefined ? [] : [{ ...resource({ key: 'terminal', label: 'Agent terminal', detail: 'Observed native runtime terminal; opening uses the app’s existing terminal view.' }), uri: terminal, glyph: 'terminal' }] }, unavailableReason: 'The selected agent runtime terminal is not observed.' },
    { title: 'Missions / queue', items: nativeQueue === undefined ? { _tag: 'Unknown' } : { _tag: 'Known', value: queuedResources }, pending: queue?._tag === 'Waiting', unavailableReason: queue === undefined ? 'Agent queue is not supplied.' : factFailure(queue) },
    { title: 'Attention', items: projected === undefined || needsMe === undefined ? { _tag: 'Unknown' } : { _tag: 'Known', value: [
      resource({ key: 'attention', label: needsMe ? 'Needs your attention' : 'No attention action observed', detail: `${projected.ask._tag === 'Known' ? `Native ask · ${projected.ask.value}` : 'Native ask unreported'}${row === undefined || Option.isNone(row.reason) ? '' : ` · ${row.reason.value}`} · same waiting-status and attention-subject union as the sidebar; read-only, not an answer action.` }),
    ] }, unavailableReason: 'Selected-agent attention state is not observed.' },
    ...(['Pull requests', 'CI runs', 'Reviews', 'Files touched', 'History'] as const).map(title => ({ title, items: { _tag: 'Unknown' as const }, unavailableReason: `${title} observations are not supplied by this native Details feed.` })),
  ]
  const nextChangeAt = earliestChange(now, [
    elapsedChange(session === undefined ? undefined : DateTime.toEpochMillis(session.started_at), now),
    session === undefined ? undefined : Option.match(session.ended_at, { onNone: () => undefined, onSome: at => DateTime.toEpochMillis(at) }),
    elapsedChange(statusSince === undefined ? undefined : DateTime.toEpochMillis(statusSince), now),
    contextAsOf,
    projected?.usage._tag === 'Known' && projected.usage.value.context !== undefined ? DateTime.toEpochMillis(projected.usage.value.context.observed_at_unix_ms) : undefined,
  ])
  return {
    overview: { title: row?.name, status: projected?.status, activity: projected?.activity, machine: projected?.host, harness: projected?.harness, model, mission,
      durations: [
        { label: 'Managed-session elapsed (from session start to end or now; wall time, not active)', value: timingPending ? 'Loading' : timing.elapsedMs === undefined ? 'Unreported' : durationText(timing.elapsedMs), pending: timingPending, ...(timing.elapsedMs === undefined && timingDetail !== undefined ? { detail: timingDetail } : {}) },
        { label: 'Current-incarnation session age (from managed session start; native launch not reported)', value: timingPending ? 'Loading' : timing.sessionAgeMs === undefined ? 'Unreported' : durationText(timing.sessionAgeMs), pending: timingPending, ...(timing.sessionAgeMs === undefined && timingDetail !== undefined ? { detail: timingDetail } : {}) },
      ],
      since: statusSince === undefined || DateTime.toEpochMillis(statusSince) > now ? undefined : durationText(now - DateTime.toEpochMillis(statusSince)),
      age: 'Incarnation launch age unreported',
    }, sections,
    trace: { agentId: agentRef, nativeSessionId: attributedRoot === undefined ? unknown('No unique usage root proven by a current-incarnation native binding.', 'not-attributed') : known(attributedRoot),
      meters: { context: { reportedPercent: occupancy._tag === 'Known' ? known(occupancy.usedPercent, occupancy.observedAt) : unknown('Context occupancy not reported for this incarnation fence.'), usedTokens: occupancy._tag === 'Known' ? contextField(occupancy.usedTokens) : unknown('Context tokens not reported.'), windowTokens: occupancy._tag === 'Known' ? contextField(occupancy.windowTokens) : unknown('Context capacity not reported.'), compactions: contextField(context?.compactions), lastCompactionMs: contextField(context?.last_compaction_ms === undefined ? undefined : Duration.toMillis(context.last_compaction_ms)) },
        spend: { usdScope: { _tag: 'IncludingSubagents' }, usdWindow: unknown('Descendant period not reported.'), totalUsdCoverage: { _tag: 'Unavailable', reason: 'not-covered' }, totalUsd: unsupported,
          mainUsdScope: { _tag: 'Self' }, mainUsdWindow: nativeWindow, mainUsdCoverage: nativeCoverage, mainUsd: hasPeriod ? known(cost, period?.until_ms) : unknown('No native period rows attributed to the selected root.', 'not-attributed'),
          subagentUsdScope: { _tag: 'IncludingSubagents' }, subagentUsdWindow: unknown('Descendant period not reported.'), subagentUsdCoverage: { _tag: 'Unavailable', reason: 'not-covered' }, subagentUsd: unsupported,
          tokensScope: { _tag: 'AgentIncarnations' }, tokensWindow: unknown('Agent cumulative token accumulation start/window is not reported.'), tokens: cumulativeUsage === undefined ? unknown('Agent cumulative tokens are not reported.') : known(cumulativeUsage.total_tokens), split: unknown('Native period does not report cost-class split.') } },
      coverage: { _tag: 'Unavailable', reason: 'not-covered' }, series: unknown('Native historical context/cost series is unsupported.'), turns: unknown('Native per-turn context series is unsupported.'), history: unknown('Historical incarnation trace is unsupported.'), subagents: unknown('Native leased subagents are resources, not a traced-session breakdown.'), subagentsWindow: unknown('Native traced-descendant window is unsupported.'), subagentCount: unknown('Native traced-descendant count is unsupported.'),
    }, state: rowFeed._tag === 'Waiting' ? 'loading' : rowFeed._tag === 'Unavailable' ? 'error' : row === undefined ? 'absent' : stale || !hasPeriod ? 'partial' : projected?.activity === 'working' ? 'busy' : 'idle',
    stale, ...(row === undefined ? {} : { observedAt: DateTime.toEpochMillis(row.updated_at) }), ...(root === undefined ? {} : { nativeSessionId: root }),
    nativeRootPending: rowFeed._tag === 'Waiting' || periodFeed._tag === 'Waiting',
    ...(incarnationId === undefined ? {} : { incarnationId }), ...(nativeHost === undefined ? {} : { host: nativeHost }), ...(projected?.harness === undefined ? {} : { harness: projected.harness }),
    ...(syncStatus === undefined ? {} : { syncStatus }), onRetry,
    ...(nextChangeAt === undefined ? {} : { nextChangeAt }),
  }
}
