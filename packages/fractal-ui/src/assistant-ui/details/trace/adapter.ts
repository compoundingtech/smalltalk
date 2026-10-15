import type { TraceFact, TraceSeries } from '../trace-series.ts'
import type { LaneSample } from './projection.ts'

/** Drawing projection only: buckets are not fabricated request rows or lifecycle facts. */
export interface ChartLane {
  readonly session: { readonly id: string; readonly parentId: string | null; readonly startedAt: number }
  readonly samples: readonly (Pick<LaneSample, 'ts' | 'isContextEstimated' | 'cumulativeUsdByComponent'> & {readonly contextPercent:number|null})[]
  /** Actual retained main-session requests, independent of time buckets. */
  readonly turnSamples?: readonly { readonly turnIndex: number; readonly ts: number; readonly contextPercent: number | null; readonly isContextEstimated: boolean }[]
  /** Present only for bucket series; never a request/turn cadence. */
  readonly bucketMs?: number
  readonly cumulativeValues: readonly { readonly ts: number; readonly usd: number }[]
  readonly totalUsd: TraceFact<number>
  readonly contextSource: 'measured'|'estimated'|'mixed'
  readonly compactions: readonly { readonly ts: number; readonly before: number | null; readonly after: number | null }[]
}
export interface ChartScene {
  readonly model: { readonly windowStart: number; readonly windowEnd: number }
  readonly lanes: readonly ChartLane[]
}
export interface TraceChartProjection {
  readonly scene: ChartScene
  readonly lane: ChartLane
  readonly descendantSpend: { readonly totalUsd: TraceFact<number>; readonly cumulativeValues: readonly { readonly ts: number; readonly usd: number }[] }
}
const chartProjections=new WeakMap<TraceSeries,TraceChartProjection>()

export const adaptTrace = (trace: TraceSeries): TraceChartProjection | undefined => {
  const cached=chartProjections.get(trace)
  if(cached!==undefined)return cached
  const series = trace.series._tag === 'Known' ? trace.series.value : undefined
  const turns = trace.turns._tag === 'Known' ? trace.turns.value : undefined
  const points = series?.points.toSorted((a, b) => a.tMs - b.tMs) ?? []
  const turnSamples = turns?.points.toSorted((a, b) => a.tMs - b.tMs || a.turnIndex - b.turnIndex).map((point) => ({
    turnIndex: point.turnIndex, ts: point.tMs, contextPercent: point.contextPercent, isContextEstimated: point.estimated,
  }))
  if (points.length === 0 && (turnSamples?.length ?? 0) === 0) return undefined
  const bucketMs = series?.bucketMs
  const windowStart = Math.min(points[0]?.tMs ?? Infinity, turnSamples?.[0]?.ts ?? Infinity)
  const windowEnd = Math.max(points.length === 0 ? -Infinity : points.at(-1)!.tMs + series!.bucketMs, turnSamples?.at(-1)?.ts ?? -Infinity)
  const id = trace.nativeSessionId._tag === 'Known' ? trace.nativeSessionId.value : trace.agentId
  let input = 0, cacheRead = 0, cacheWrite = 0, output = 0, descendants = 0
  const cumulativeValues: { ts: number; usd: number }[] = []
  const descendantValues: { ts: number; usd: number }[] = []
  const samples = points.flatMap((point) => {
    input += point.cost.uncachedInputUsd
    cacheRead += point.cost.cacheReadUsd
    cacheWrite += point.cost.cacheWriteUsd
    output += point.cost.outputUsd
    descendants += point.subagentUsd
    // Role-only buckets contain additive zeros, not main-session observations.
    const hasMainObservation = point.contextTokens !== null || point.contextCapacity !== null || Object.values(point.tokens).some(value => value !== 0) || Object.values(point.cost).some(value => value !== 0)
    if (hasMainObservation) cumulativeValues.push({ ts: point.tMs, usd: input + cacheRead + cacheWrite + output })
    if (point.subagentUsd !== 0) descendantValues.push({ ts: point.tMs, usd: descendants })
    const contextTokens=point.contextTokens??(point.contextCapacity===null?null:point.tokens.input+point.tokens.cacheRead+point.tokens.cacheWrite)
    return hasMainObservation ? [{ ts: point.tMs, contextPercent: contextTokens!==null&&point.contextCapacity!==null&&point.contextCapacity>0?contextTokens/point.contextCapacity*100:null, isContextEstimated: point.contextTokens===null&&point.contextCapacity!==null, cumulativeUsdByComponent: { input, cacheRead, cacheWrite, output } }] : []
  })
  const contextSamples = points.length > 0 ? samples : turnSamples ?? []
  const compactions = points.length > 0 ? series!.compactions : turns?.compactions ?? series?.compactions ?? []
  const lane: ChartLane = {
    session: { id, parentId: null, startedAt: windowStart }, samples, cumulativeValues,
    ...(turnSamples === undefined ? {} : { turnSamples }),
    ...(bucketMs === undefined ? {} : { bucketMs }),
    totalUsd: trace.meters.spend.mainUsd,
    contextSource: contextSamples.every(sample=>!sample.isContextEstimated)?'measured':contextSamples.every(sample=>sample.isContextEstimated)?'estimated':'mixed',
    compactions: compactions.filter((event) => event.sessionId === id).map((event) => ({
      ts: event.tMs,
      // A floored bucket can contain observations from either side of the event.
      // Only a bucket whose complete interval precedes the event proves "before".
      before: points.length > 0
        ? samples.findLast((sample) => sample.ts + series!.bucketMs <= event.tMs && sample.contextPercent !== null)?.contextPercent ?? null
        : turnSamples?.findLast((sample) => sample.ts < event.tMs)?.contextPercent ?? null,
      after: points.length > 0
        ? samples.find((sample) => sample.ts > event.tMs && sample.contextPercent !== null)?.contextPercent ?? null
        : turnSamples?.find((sample) => sample.ts > event.tMs)?.contextPercent ?? null,
    })),
  }
  const projection:TraceChartProjection={
    scene: { model: { windowStart, windowEnd }, lanes: [lane] }, lane,
    descendantSpend: { totalUsd: trace.meters.spend.subagentUsd, cumulativeValues: descendantValues },
  }
  chartProjections.set(trace,projection)
  return projection
}
