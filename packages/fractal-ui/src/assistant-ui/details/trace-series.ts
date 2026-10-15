/** Portable projection; the schema and Effect provider live in separate modules. */
export type TraceUnknownReason = 'no-provider'|'not-covered'|'not-attributed'|'out-of-retention'|'loading'|'failed'
export type TraceFact<A> = {readonly _tag:'Known';readonly value:A;readonly source:'st'|'series';readonly asOfMs?:number}|{readonly _tag:'Unknown';readonly reason:TraceUnknownReason;readonly detail?:string}
export type TraceSpendScope = {readonly _tag:'Self'}|{readonly _tag:'IncludingSubagents'}|{readonly _tag:'AgentIncarnations'}|{readonly _tag:'Unknown';readonly reason:'not-attributed'}
export type TraceCoverage = {readonly _tag:'Complete'}|{readonly _tag:'Partial';readonly attributedFraction:number}|{readonly _tag:'NotCovered';readonly harnessName:string}|{readonly _tag:'Unavailable';readonly reason:TraceUnknownReason}
/** Per-row presentation state; malformed direct props do not invalidate sibling facts. */
export type TraceCoverageState = TraceCoverage | {readonly _tag:'PartialUnreported'}
export const traceCoverageState=(coverage:TraceCoverageState):TraceCoverageState=>
 coverage._tag==='Partial'&&(!Number.isFinite(coverage.attributedFraction)||coverage.attributedFraction<0||coverage.attributedFraction>1)
  ? {_tag:'PartialUnreported'} : coverage
/** Reported accumulation window for each metric, independent of chart retention. */
export type TraceAccumulationWindow = {readonly _tag:'AllTime'}|{readonly _tag:'Interval';readonly fromMs:number;readonly toMs:number}|{readonly _tag:'Reported';readonly label:string}
export interface TraceCostSplit {readonly uncachedInputUsd:number;readonly cacheReadUsd:number;readonly cacheWriteUsd:number;readonly outputUsd:number}
export interface TraceBucket {readonly tMs:number;readonly tokens:{readonly input:number;readonly output:number;readonly cacheRead:number;readonly cacheWrite:number};readonly cost:TraceCostSplit;readonly model:string;readonly contextTokens:number|null;readonly contextCapacity:number|null;readonly compactionEpoch:number|null;readonly subagentUsd:number;readonly requests:number}
export interface TraceBuckets {readonly range:'1d'|'7d'|'30d';readonly bucketMs:number;readonly points:readonly TraceBucket[];readonly compactions:readonly {readonly tMs:number;readonly sessionId:string}[];readonly coverage:{readonly harnesses:readonly ('omp'|'claude'|'codex')[];readonly requestsIncluded:number;readonly requestsTotal:number;readonly oldestMs:number}}
/** Retained main-session request observations, never expanded from bucket counts. */
export interface TraceTurns {readonly points:readonly {readonly turnIndex:number;readonly tMs:number;readonly contextTokens:number|null;readonly contextPercent:number|null;readonly estimated:boolean}[];readonly compactions:readonly {readonly tMs:number;readonly sessionId:string}[]}
export interface TraceSubagent {readonly sessionId:string;readonly parentSessionId:TraceFact<string>;readonly label:TraceFact<string>;readonly model:TraceFact<string>;readonly usd:number;readonly requests:number;readonly firstMs:TraceFact<number>;readonly lastMs:TraceFact<number>}
export interface TraceIncarnation {readonly nativeSessionId:string;readonly startedMs:number;readonly endedMs:number|null;readonly usd:number;readonly compactions:number}
export interface TraceSpendMeters {
 readonly usdScope:TraceSpendScope
 readonly usdWindow:TraceFact<TraceAccumulationWindow>
 readonly totalUsdCoverage:TraceCoverage
 readonly mainUsdScope:TraceSpendScope
 readonly mainUsdWindow:TraceFact<TraceAccumulationWindow>
 readonly mainUsdCoverage:TraceCoverage
 readonly subagentUsdScope:TraceSpendScope
 readonly subagentUsdWindow:TraceFact<TraceAccumulationWindow>
 readonly subagentUsdCoverage:TraceCoverage
 readonly tokensScope:TraceSpendScope
 readonly tokensWindow:TraceFact<TraceAccumulationWindow>
 readonly totalUsd:TraceFact<number>
 readonly mainUsd:TraceFact<number>
 readonly subagentUsd:TraceFact<number>
 readonly tokens:TraceFact<number>
 readonly split:TraceFact<TraceCostSplit>
}
export interface TraceSeries {
 readonly agentId:string
 readonly nativeSessionId:TraceFact<string>
 readonly meters:{readonly context:{readonly reportedPercent:TraceFact<number>;readonly usedTokens:TraceFact<number>;readonly windowTokens:TraceFact<number>;readonly compactions:TraceFact<number>;readonly lastCompactionMs:TraceFact<number>};readonly spend:TraceSpendMeters}
 readonly coverage:TraceCoverage
 readonly series:TraceFact<TraceBuckets>
 readonly turns:TraceFact<TraceTurns>
 readonly subagents:TraceFact<readonly TraceSubagent[]>
 readonly subagentsWindow:TraceFact<TraceAccumulationWindow>
 readonly subagentCount:TraceFact<number>
 readonly history:TraceFact<readonly TraceIncarnation[]>
}

/** A reported percentage is usable without underlying token counts. */
export const contextPercent = (context: TraceSeries['meters']['context']): number | undefined => {
 if(context.reportedPercent._tag==='Known')return context.reportedPercent.value
 return context.usedTokens._tag==='Known'&&context.windowTokens._tag==='Known'&&context.windowTokens.value>0?context.usedTokens.value/context.windowTokens.value*100:undefined
}
