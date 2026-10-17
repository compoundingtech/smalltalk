import type { DetailsOverview, TraceSeries, TraceCoverage } from '@smalltalk/fractal-ui/assistant-ui'
import type { Feed } from '../data/source.ts'
import type { DetailsSupplement } from '../extensions/contract.ts'

const harnessName = (harness: string | undefined): string =>
  harness === 'claude' ? 'Claude Code' : harness === 'codex' ? 'Codex' : harness ?? 'this harness'

/** Merge only a proven matching root. Native money/tokens and provider windows remain independent. */
export const detailsWithSupplement = ({ trace, overview, supplement, harness, nativeSessionId }: {
  readonly trace: TraceSeries
  readonly overview: DetailsOverview
  readonly supplement: Feed<DetailsSupplement>
  readonly harness?: string
  readonly nativeSessionId?: string
}): { readonly trace: TraceSeries; readonly overview: DetailsOverview; readonly accountingNote?: string } => {
  if (harness !== 'omp') {
    const coverage: TraceCoverage = harness === undefined
      ? { _tag: 'Unavailable', reason: 'not-attributed' }
      : { _tag: 'NotCovered', harnessName: harnessName(harness) }
    return {
      overview,
      trace: {
        ...trace,
        coverage,
        meters: {
          ...trace.meters,
          spend: { ...trace.meters.spend, totalUsdCoverage: coverage, subagentUsdCoverage: coverage },
        },
        subagents: { _tag: 'Unknown', reason: harness === undefined ? 'not-attributed' : 'not-covered' },
        subagentsWindow: { _tag: 'Unknown', reason: 'not-covered' },
        subagentCount: { _tag: 'Unknown', reason: 'not-covered' },
      },
    }
  }
  if (supplement._tag !== 'Observed' || nativeSessionId === undefined ||
    trace.nativeSessionId._tag !== 'Known' || trace.nativeSessionId.value !== nativeSessionId ||
    supplement.value.nativeSessionId !== nativeSessionId) return { trace, overview }
  const value = supplement.value
  return {
    ...(trace.meters.spend.mainUsd._tag === 'Known' && value.totalUsd._tag === 'Known' ? {
      accountingNote: `${value.subagentCount._tag === 'Known' && value.subagentCount.value === 0 ? 'Reported traced descendants: 0. ' : ''}Native self and ledger inclusive use separate accounting sources/windows, not a reconciled split. Their difference is not descendant spend.`,
    } : {}),
    overview: value.durations === undefined ? overview : {
      ...overview,
      durations: [...(overview.durations ?? []), ...value.durations],
    },
    trace: {
      ...trace,
      coverage: value.coverage,
      meters: {
        ...trace.meters,
        spend: {
          ...trace.meters.spend,
          usdScope: { _tag: 'IncludingSubagents' },
          usdWindow: value.usdWindow,
          totalUsd: value.totalUsd,
          totalUsdCoverage: value.coverage,
          // An inclusive observed total does not establish a matching main/descendant split.
        },
      },
      subagents: value.subagents,
      subagentsWindow: value.subagentsWindow,
      subagentCount: value.subagentCount,
    },
  }
}
