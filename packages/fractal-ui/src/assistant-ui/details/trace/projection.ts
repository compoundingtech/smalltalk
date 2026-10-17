import type {
  AgentTraceBin,
  AgentTraceContextSegment,
  AgentTraceHumanPrompt,
  AgentTraceModel,
  AgentTraceRequest,
  AgentTraceSession,
} from './model.ts'
import type { ActivitySegment } from './types.ts'

/** Per-evidence-point lane metrics, with epoch-millisecond timing and cumulative USD totals. */
export interface LaneSample {
  readonly ts: number
  readonly contextTokens: number | null
  readonly contextFraction: number | null
  readonly isContextEstimated: boolean
  readonly cumulativeUsd: number
  readonly cumulativeUsdByComponent: {
    readonly input: number
    readonly cacheRead: number
    readonly cacheWrite: number
    readonly output: number
  }
  readonly usd: number
  readonly requests: number
  readonly isAgentTurn: boolean
  readonly toolCalls: number
}

/**
 * Min–max context fill of one folded compaction segment, spanning the epoch-millisecond times of
 * its first and last request. Fractions are per request and may exceed 1 on an overfull window.
 */
export interface LaneContextBand {
  readonly from: number
  readonly to: number
  readonly compactionEpoch: number
  readonly minFraction: number
  readonly maxFraction: number
  /** The bin derived context tokens from input/cache counts instead of a measured snapshot. */
  readonly estimated: boolean
  /** Some requests in the segment have unknown capacity, so the extent covers only the rest. */
  readonly partialCapacity: boolean
}

/** Lifecycle state that determines how a lane's open or closed endpoint is rendered. */
export type LaneStatus =
  | { readonly _tag: 'exited'; readonly at: number }
  | { readonly _tag: 'active'; readonly lastRequestAt: number }
  | { readonly _tag: 'inactive'; readonly lastRequestAt: number }

/** Render-ready projection of one session and its cumulative activity, cost, and token metrics. */
export interface Lane {
  readonly session: AgentTraceSession
  readonly rootId: string
  readonly depth: number
  readonly order: number
  readonly samples: ReadonlyArray<LaneSample>
  readonly bins: ReadonlyArray<AgentTraceBin>
  /** Hatched min–max context bands from folded bins; segments without any known capacity are omitted. */
  readonly contextBands: ReadonlyArray<LaneContextBand>
  readonly cumulativeValues: ReadonlyArray<{
    readonly ts: number
    readonly usd: number
    readonly requests: number
  }>
  readonly contextCapacity: number | null
  readonly contextSource: 'measured' | 'estimated' | 'mixed'
  readonly activity: ReadonlyArray<ActivitySegment>
  readonly wallMs: number
  readonly activeMs: number
  readonly compactionCount: number
  readonly compactions: ReadonlyArray<{
    readonly ts: number
    readonly before: number | null
    readonly after: number | null
  }>
  readonly humanPrompts: ReadonlyArray<AgentTraceHumanPrompt>
  readonly humanPromptCount: number
  readonly resumptions: ReadonlyArray<{ readonly exitedAt: number; readonly resumedAt: number }>
  readonly models: ReadonlyArray<{ readonly from: number; readonly model: string }>
  readonly currentModel: string
  readonly spendByModel: Readonly<Record<string, number>>
  readonly totalUsd: number
  readonly tokens: {
    readonly input: number
    readonly cacheRead: number
    readonly cacheWrite: number
    readonly output: number
    readonly total: number
  }
  readonly agentTurns: number
  readonly requests: number
  readonly toolCalls: number
  readonly status: LaneStatus
  readonly endTs: number
}

/** Parent-child session edge anchored at the child's epoch-millisecond start timestamp. */
export interface ForkEdge {
  readonly parentSessionId: string
  readonly childSessionId: string
  readonly ts: number
}

/** Tree-ordered rendering projection and aggregate totals derived from an Agent Trace model. */
export interface AgentTraceScene {
  readonly model: AgentTraceModel
  readonly lanes: ReadonlyArray<Lane>
  readonly forks: ReadonlyArray<ForkEdge>
  readonly totals: {
    readonly usd: number
    readonly requests: number
    readonly toolCalls: number
    readonly agentTurns: number
    readonly humanPrompts: number
    readonly sessions: number
    readonly active: number
  }
  readonly spendByModel: Readonly<Record<string, number>>
  readonly cumulativeUsd: ReadonlyArray<{ readonly ts: number; readonly usd: number }>
}
const sampleLastTool = (sample: Lane['samples'][number]): string | null => {
  if ('lastTool' in sample && typeof sample.lastTool === 'string') return sample.lastTool
  if ('tools' in sample && Array.isArray(sample.tools)) {
    const last = sample.tools.at(-1)
    return typeof last === 'string' ? last : null
  }
  return null
}

/** Read cumulative lane values at a cursor timestamp with one upper-bound search. */
export const valuesAt = (
  lane: Lane,
  ts: number,
): {
  contextTokens: number | null
  isContextEstimated: boolean
  cumulativeUsd: number
  requests: number
  lastTool: string | null
  model: string
  modelChangedAt: number | null
  isAfterCompaction: { ts: number; before: number | null; after: number | null } | null
} => {
  let low = 0
  let high = lane.samples.length
  while (low < high) {
    const middle = low + Math.floor((high - low) / 2)
    if (lane.samples[middle]!.ts <= ts) low = middle + 1
    else high = middle
  }
  const sampleIndex = low - 1
  const sample = sampleIndex < 0 ? undefined : lane.samples[sampleIndex]

  let cumulativeLow = 0
  let cumulativeHigh = lane.cumulativeValues.length
  while (cumulativeLow < cumulativeHigh) {
    const middle = cumulativeLow + Math.floor((cumulativeHigh - cumulativeLow) / 2)
    if (lane.cumulativeValues[middle]!.ts <= ts) cumulativeLow = middle + 1
    else cumulativeHigh = middle
  }
  const cumulative = cumulativeLow === 0 ? undefined : lane.cumulativeValues[cumulativeLow - 1]

  let compactionLow = 0
  let compactionHigh = lane.compactions.length
  while (compactionLow < compactionHigh) {
    const middle = compactionLow + Math.floor((compactionHigh - compactionLow) / 2)
    if (lane.compactions[middle]!.ts <= ts) compactionLow = middle + 1
    else compactionHigh = middle
  }
  const modelEntry = lane.models.findLast((entry) => entry.from <= ts)

  return {
    contextTokens: sample?.contextTokens ?? null,
    isContextEstimated: sample?.isContextEstimated ?? false,
    cumulativeUsd: cumulative?.usd ?? 0,
    requests: cumulative?.requests ?? 0,
    lastTool: sample === undefined ? null : sampleLastTool(sample),
    model: modelEntry?.model ?? lane.session.model,
    modelChangedAt:
      modelEntry !== undefined && modelEntry.from > lane.session.startedAt ? modelEntry.from : null,
    isAfterCompaction: compactionLow === 0 ? null : lane.compactions[compactionLow - 1]!,
  }
}

const TEN_MINUTES_MS = 10 * 60 * 1_000

const byStartedAt = (left: AgentTraceSession, right: AgentTraceSession): number =>
  left.startedAt - right.startedAt

const contextTokensFor = (request: AgentTraceRequest): number =>
  request.contextTokens ?? request.inputTokens + request.cacheReadTokens + request.cacheWriteTokens

const capacityFor = (
  request: AgentTraceRequest,
  modelCapacities: Readonly<Record<string, number>>,
): number | undefined => request.contextCapacity ?? modelCapacities[request.model]

const bandFor = (
  bin: AgentTraceBin,
  segment: AgentTraceContextSegment,
): ReadonlyArray<LaneContextBand> =>
  segment.minFraction === null || segment.maxFraction === null
    ? []
    : [
        {
          from: segment.from,
          to: segment.to,
          compactionEpoch: segment.compactionEpoch,
          minFraction: segment.minFraction,
          maxFraction: segment.maxFraction,
          estimated: bin.contextEstimated,
          partialCapacity: segment.unknown > 0,
        },
      ]

/**
 * Orders every resident session as roots-by-start-time followed by each root's
 * descendants in depth-first, sibling-start-time order.
 */
const orderSessions = (
  sessions: ReadonlyArray<AgentTraceSession>,
): ReadonlyArray<AgentTraceSession> => {
  const byId = new Map(sessions.map((session) => [session.id, session]))
  const children = new Map<string, AgentTraceSession[]>()
  const roots: AgentTraceSession[] = []

  for (const session of sessions) {
    if (session.parentId === null || !byId.has(session.parentId)) {
      roots.push(session)
      continue
    }
    const siblings = children.get(session.parentId) ?? []
    siblings.push(session)
    children.set(session.parentId, siblings)
  }

  const ordered: AgentTraceSession[] = []
  const visited = new Set<string>()
  const visit = (session: AgentTraceSession): void => {
    if (visited.has(session.id)) return
    visited.add(session.id)
    ordered.push(session)
    for (const child of (children.get(session.id) ?? []).toSorted(byStartedAt)) visit(child)
  }

  for (const root of roots.toSorted(byStartedAt)) visit(root)
  for (const session of sessions.toSorted(byStartedAt)) visit(session)
  return ordered
}

const mergeActivitySegments = (
  segments: ReadonlyArray<ActivitySegment>,
): ReadonlyArray<ActivitySegment> => {
  const merged: ActivitySegment[] = []
  for (const segment of segments) {
    if (segment.to <= segment.from) continue
    const previous = merged.at(-1)
    if (
      previous !== undefined &&
      previous.state === segment.state &&
      previous.to === segment.from
    ) {
      merged[merged.length - 1] = { ...previous, to: segment.to }
    } else {
      merged.push(segment)
    }
  }
  return merged
}

/**
 * Derives activity from complete request rows. Extractors call this before
 * thinning; projection calls it only as a compatibility fallback.
 */
export const deriveAgentTraceActivity = ({
  sessions,
  requests,
  windowStart,
  windowEnd,
  idleThresholdMs = TEN_MINUTES_MS,
}: {
  readonly sessions: ReadonlyArray<AgentTraceSession>
  readonly requests: ReadonlyArray<AgentTraceRequest>
  readonly windowStart: number
  readonly windowEnd: number
  readonly idleThresholdMs?: number
}): ReadonlyMap<string, ReadonlyArray<ActivitySegment>> => {
  const requestsBySession = new Map<string, number[]>()
  for (const request of requests) {
    if (request.ts < windowStart || request.ts > windowEnd) continue
    const timestamps = requestsBySession.get(request.sessionId) ?? []
    timestamps.push(request.ts)
    requestsBySession.set(request.sessionId, timestamps)
  }
  const threshold = Math.max(0, idleThresholdMs)
  for (const [sessionId, timestamps] of requestsBySession)
    requestsBySession.set(
      sessionId,
      timestamps.toSorted((left, right) => left - right),
    )

  const childrenBySession = new Map<string, string[]>()
  for (const session of sessions) {
    if (session.parentId === null) continue
    const children = childrenBySession.get(session.parentId) ?? []
    children.push(session.id)
    childrenBySession.set(session.parentId, children)
  }

  const result = new Map<string, ReadonlyArray<ActivitySegment>>()
  for (const session of sessions) {
    const sessionRequests = requestsBySession.get(session.id) ?? []
    if (sessionRequests.length < 2) {
      result.set(session.id, [])
      continue
    }

    const descendants = new Set<string>()
    const visit = (parentId: string): void => {
      for (const childId of childrenBySession.get(parentId) ?? []) {
        if (descendants.has(childId)) continue
        descendants.add(childId)
        visit(childId)
      }
    }
    visit(session.id)
    const descendantActivity = [...descendants]
      .flatMap((sessionId) => requestsBySession.get(sessionId) ?? [])
      .toSorted((left, right) => left - right)

    const segments: ActivitySegment[] = []
    let descendantIndex = 0
    for (let index = 0; index < sessionRequests.length - 1; index += 1) {
      const from = sessionRequests[index]!
      const to = sessionRequests[index + 1]!
      if (to <= from) continue
      if (to - from <= threshold) {
        segments.push({ from, to, state: 'working' })
        continue
      }
      while (
        descendantActivity[descendantIndex] !== undefined &&
        descendantActivity[descendantIndex]! <= from
      ) {
        descendantIndex += 1
      }
      segments.push({
        from,
        to,
        state:
          (descendantActivity[descendantIndex] ?? Number.POSITIVE_INFINITY) < to
            ? 'waiting'
            : 'idle',
      })
    }
    result.set(session.id, mergeActivitySegments(segments))
  }
  return result
}

/**
 * Finds the nearest context snapshots strictly before and after a compaction.
 * The strict bounds mirror the ledger's `context_lifecycle` lens: samples at
 * the exact event timestamp belong to neither side.
 */
const compactionBounds = (
  ts: number,
  samples: ReadonlyArray<LaneSample>,
): { readonly before: number | null; readonly after: number | null } => {
  let beforeTs = Number.NEGATIVE_INFINITY
  let before: number | null = null
  let afterTs = Number.POSITIVE_INFINITY
  let after: number | null = null

  for (const sample of samples) {
    if (sample.ts < ts && sample.ts > beforeTs) {
      beforeTs = sample.ts
      before = sample.contextTokens
    }
    if (sample.ts > ts && sample.ts < afterTs) {
      afterTs = sample.ts
      after = sample.contextTokens
    }
  }

  return { before, after }
}

const addSpend = (target: Record<string, number>, model: string, usd: number): void => {
  target[model] = (target[model] ?? 0) + usd
}

type Evidence =
  | { readonly _tag: 'request'; readonly ts: number; readonly request: AgentTraceRequest }
  | { readonly _tag: 'bin'; readonly ts: number; readonly bin: AgentTraceBin }

const byEvidenceTime = (left: Evidence, right: Evidence): number =>
  left.ts - right.ts || (left._tag === 'bin' ? -1 : 1)

const evidenceFor = (
  requests: ReadonlyArray<AgentTraceRequest>,
  bins: ReadonlyArray<AgentTraceBin>,
): ReadonlyArray<Evidence> =>
  [
    ...requests.map((request): Evidence => ({ _tag: 'request', ts: request.ts, request })),
    ...bins.map((bin): Evidence => ({ _tag: 'bin', ts: bin.to, bin })),
  ].toSorted(byEvidenceTime)

/** Project a ledger-shaped model into tree-ordered rendering lanes and totals. */
export const projectAgentTrace = (model: AgentTraceModel): AgentTraceScene => {
  const orderedSessions = orderSessions(model.sessions)
  const requestsBySession = new Map<string, AgentTraceRequest[]>()
  for (const request of model.requests) {
    const requests = requestsBySession.get(request.sessionId) ?? []
    requests.push(request)
    requestsBySession.set(request.sessionId, requests)
  }
  for (const [sessionId, requests] of requestsBySession)
    requestsBySession.set(
      sessionId,
      requests.toSorted((left, right) => left.ts - right.ts),
    )

  const binsBySession = new Map<string, AgentTraceBin[]>()
  for (const bin of model.bins) {
    const bins = binsBySession.get(bin.sessionId) ?? []
    bins.push(bin)
    binsBySession.set(bin.sessionId, bins)
  }
  for (const [sessionId, bins] of binsBySession)
    binsBySession.set(
      sessionId,
      bins.toSorted((left, right) => left.from - right.from),
    )

  const compactionsBySession = new Map<string, number[]>()
  const modelChangesBySession = new Map<
    string,
    Array<{ readonly from: number; readonly model: string }>
  >()
  const exitTimestampsBySession = new Map<string, number[]>()
  for (const event of model.events) {
    if (event._tag === 'session_exit') {
      const timestamps = exitTimestampsBySession.get(event.sessionId) ?? []
      timestamps.push(event.ts)
      exitTimestampsBySession.set(event.sessionId, timestamps)
    } else if (event._tag === 'compaction') {
      const timestamps = compactionsBySession.get(event.sessionId) ?? []
      timestamps.push(event.ts)
      compactionsBySession.set(event.sessionId, timestamps)
    } else {
      const changes = modelChangesBySession.get(event.sessionId) ?? []
      changes.push({ from: event.ts, model: event.model })
      modelChangesBySession.set(event.sessionId, changes)
    }
  }
  for (const [sessionId, timestamps] of compactionsBySession)
    compactionsBySession.set(
      sessionId,
      timestamps.toSorted((left, right) => left - right),
    )
  for (const [sessionId, timestamps] of exitTimestampsBySession)
    exitTimestampsBySession.set(
      sessionId,
      timestamps.toSorted((left, right) => left - right),
    )
  for (const [sessionId, changes] of modelChangesBySession)
    modelChangesBySession.set(
      sessionId,
      changes.toSorted((left, right) => left.from - right.from),
    )

  const promptsBySession = new Map<string, AgentTraceHumanPrompt[]>()
  for (const prompt of model.humanPrompts) {
    const prompts = promptsBySession.get(prompt.sessionId) ?? []
    prompts.push(prompt)
    promptsBySession.set(prompt.sessionId, prompts)
  }
  for (const [sessionId, prompts] of promptsBySession)
    promptsBySession.set(
      sessionId,
      prompts.toSorted((left, right) => left.ts - right.ts),
    )

  const fallbackActivityBySession = deriveAgentTraceActivity({
    sessions: model.sessions,
    requests: model.requests,
    windowStart: model.windowStart,
    windowEnd: model.windowEnd,
    idleThresholdMs: model.parameters.idleThresholdMs,
  })

  const lanes = orderedSessions.map((session, order): Lane => {
    const requests = requestsBySession.get(session.id) ?? []
    const bins = binsBySession.get(session.id) ?? []
    const facts = session.facts
    let cumulativeUsd = 0
    let cumulativeUsdInput = 0
    let cumulativeUsdCacheRead = 0
    let cumulativeUsdCacheWrite = 0
    let cumulativeUsdOutput = 0
    let inputTokens = 0
    let cacheReadTokens = 0
    let cacheWriteTokens = 0
    let outputTokens = 0
    let cumulativeRequests = 0
    let cumulativeToolCalls = 0
    const samples: LaneSample[] = []
    const cumulativeValues: Array<{
      readonly ts: number
      readonly usd: number
      readonly requests: number
    }> = []
    for (const evidence of evidenceFor(requests, bins)) {
      const value = evidence._tag === 'request' ? evidence.request : evidence.bin
      cumulativeUsd += value.usd
      cumulativeUsdInput += value.usdInput
      cumulativeUsdCacheRead += value.usdCacheRead
      cumulativeUsdCacheWrite += value.usdCacheWrite
      cumulativeUsdOutput += value.usdOutput
      inputTokens += value.inputTokens
      cacheReadTokens += value.cacheReadTokens
      cacheWriteTokens += value.cacheWriteTokens
      outputTokens += value.outputTokens
      cumulativeRequests += value.requests
      cumulativeToolCalls += value.toolCalls
      cumulativeValues.push({ ts: evidence.ts, usd: cumulativeUsd, requests: cumulativeRequests })
      if (evidence._tag === 'bin') continue

      const { request } = evidence
      const contextTokens = contextTokensFor(request)
      const contextCapacity = capacityFor(request, model.modelCapacities)
      samples.push({
        ts: request.ts,
        contextTokens,
        contextFraction: contextCapacity === undefined ? null : contextTokens / contextCapacity,
        isContextEstimated: request.contextTokens === null,
        cumulativeUsd,
        cumulativeUsdByComponent: {
          input: cumulativeUsdInput,
          cacheRead: cumulativeUsdCacheRead,
          cacheWrite: cumulativeUsdCacheWrite,
          output: cumulativeUsdOutput,
        },
        requests: cumulativeRequests,
        usd: request.usd,
        isAgentTurn: request.kind === 'turn' && request.stopReason === 'stop',
        toolCalls: cumulativeToolCalls,
      })
    }

    const compactions = (compactionsBySession.get(session.id) ?? []).map((ts) => {
      const bounds = compactionBounds(ts, samples)
      return { ts, before: bounds.before, after: bounds.after }
    })
    const lastRequestAt =
      facts?.lastRequestAt ??
      Math.max(
        session.startedAt,
        requests.at(-1)?.ts ?? Number.NEGATIVE_INFINITY,
        bins.at(-1)?.lastTs ?? bins.at(-1)?.to ?? Number.NEGATIVE_INFINITY,
      )
    const eventExits = exitTimestampsBySession.get(session.id) ?? []
    const exitTimestamps =
      session.endedAt === null || eventExits.includes(session.endedAt)
        ? eventExits
        : [...eventExits, session.endedAt].toSorted((left, right) => left - right)
    const lastExitAt = exitTimestamps.at(-1)
    const status: LaneStatus =
      facts?.status === 'exited'
        ? { _tag: 'exited', at: facts.lastExitAt ?? lastRequestAt }
        : facts?.status === 'active'
          ? { _tag: 'active', lastRequestAt }
          : facts?.status === 'inactive'
            ? { _tag: 'inactive', lastRequestAt }
            : lastExitAt !== undefined && lastExitAt >= lastRequestAt
              ? { _tag: 'exited', at: lastExitAt }
              : lastRequestAt >= model.windowEnd - model.parameters.activeWithinMs
                ? { _tag: 'active', lastRequestAt }
                : { _tag: 'inactive', lastRequestAt }
    const resumptions = exitTimestamps.flatMap((exitedAt) => {
      const resumedAt = [
        ...requests.map((request) => request.ts),
        ...bins.map((bin) =>
          (bin.firstTs ?? bin.from) > exitedAt ? (bin.firstTs ?? bin.from) : (bin.lastTs ?? bin.to),
        ),
      ]
        .filter((timestamp) => timestamp > exitedAt)
        .toSorted((left, right) => left - right)
        .at(0)
      return resumedAt === undefined ? [] : [{ exitedAt, resumedAt }]
    })
    const endTs = status._tag === 'exited' ? Math.max(lastRequestAt, status.at) : lastRequestAt
    const rawModels = [
      { from: session.startedAt, model: session.model },
      ...(modelChangesBySession.get(session.id) ?? []),
    ].toSorted((left, right) => left.from - right.from)
    const models = rawModels.filter(
      (entry, index) => index === 0 || entry.model !== rawModels[index - 1]!.model,
    )
    const currentModel = models.at(-1)?.model ?? session.model
    const hasMeasuredContext =
      requests.some((request) => request.contextTokens !== null) ||
      bins.some((bin) => !bin.contextEstimated)
    const hasEstimatedContext =
      requests.some((request) => request.contextTokens === null) ||
      bins.some((bin) => bin.contextEstimated)
    const contextSource =
      hasMeasuredContext && hasEstimatedContext
        ? 'mixed'
        : hasEstimatedContext
          ? 'estimated'
          : 'measured'
    const activity = session.activity ?? fallbackActivityBySession.get(session.id) ?? []
    const spendByModel: Record<string, number> = {}
    for (const request of requests) addSpend(spendByModel, request.model, request.usd)
    for (const bin of bins) {
      for (const [requestModel, aggregate] of Object.entries(bin.byModel)) {
        addSpend(spendByModel, requestModel, aggregate.usd)
      }
    }

    return {
      session,
      rootId: session.rootId,
      depth: session.depth,
      order,
      samples,
      bins,
      contextBands: bins.flatMap((bin) =>
        (bin.contextSegments ?? []).flatMap((segment) => bandFor(bin, segment)),
      ),
      cumulativeValues,
      contextCapacity:
        requests.findLast(
          (request) => request.model === currentModel && request.contextCapacity !== null,
        )?.contextCapacity ??
        model.modelCapacities[currentModel] ??
        null,
      contextSource,
      activity,
      wallMs: Math.max(0, lastRequestAt - (requests.at(0)?.ts ?? session.startedAt)),
      activeMs:
        facts?.activeMs ??
        session.activeMs ??
        activity.reduce(
          (total, segment) =>
            segment.state === 'working' ? total + segment.to - segment.from : total,
          0,
        ),
      compactions,
      compactionCount: facts?.compactions ?? compactions.length,
      humanPrompts: promptsBySession.get(session.id) ?? [],
      humanPromptCount:
        facts?.prompts ??
        model.humanPromptCounts?.[session.id] ??
        (promptsBySession.get(session.id) ?? []).length,
      resumptions,
      models,
      currentModel,
      spendByModel,
      totalUsd: facts?.usd ?? cumulativeUsd,
      tokens: {
        input: facts?.inputTokens ?? inputTokens,
        cacheRead: facts?.cacheReadTokens ?? cacheReadTokens,
        cacheWrite: facts?.cacheWriteTokens ?? cacheWriteTokens,
        output: facts?.outputTokens ?? outputTokens,
        total:
          (facts?.inputTokens ?? inputTokens) +
          (facts?.cacheReadTokens ?? cacheReadTokens) +
          (facts?.cacheWriteTokens ?? cacheWriteTokens) +
          (facts?.outputTokens ?? outputTokens),
      },
      agentTurns:
        facts?.turns ??
        samples.filter((sample) => sample.isAgentTurn).length +
          bins.reduce((total, bin) => total + bin.agentTurns, 0),
      requests: facts?.requests ?? cumulativeRequests,
      toolCalls: facts?.toolCalls ?? cumulativeToolCalls,
      status,
      endTs,
    }
  })

  const residentSessionIds = new Set(model.sessions.map((session) => session.id))
  const forks = orderedSessions.flatMap((session): ForkEdge[] =>
    session.parentId !== null && residentSessionIds.has(session.parentId)
      ? [
          {
            parentSessionId: session.parentId,
            childSessionId: session.id,
            ts: session.startedAt,
          },
        ]
      : [],
  )

  let totalUsd = 0
  const spendByModel: Record<string, number> = {}
  const cumulativeUsd = evidenceFor(model.requests, model.bins).map((evidence) => {
    if (evidence._tag === 'request') {
      totalUsd += evidence.request.usd
      addSpend(spendByModel, evidence.request.model, evidence.request.usd)
    } else {
      totalUsd += evidence.bin.usd
      for (const [requestModel, aggregate] of Object.entries(evidence.bin.byModel)) {
        addSpend(spendByModel, requestModel, aggregate.usd)
      }
    }
    return { ts: evidence.ts, usd: totalUsd }
  })

  return {
    model,
    lanes,
    forks,
    totals: {
      usd: lanes.reduce((sum, lane) => sum + lane.totalUsd, 0),
      requests: lanes.reduce((sum, lane) => sum + lane.requests, 0),
      toolCalls: lanes.reduce((sum, lane) => sum + lane.toolCalls, 0),
      agentTurns: lanes.reduce((sum, lane) => sum + lane.agentTurns, 0),
      humanPrompts: lanes.reduce((sum, lane) => sum + lane.humanPromptCount, 0),
      sessions: lanes.length,
      active: lanes.filter((lane) => lane.status._tag === 'active').length,
    },
    spendByModel,
    cumulativeUsd,
  }
}
