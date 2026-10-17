import { Schema } from 'effect'
import type { TraceSeries } from './trace-series'
const count=Schema.Int.check(Schema.isGreaterThanOrEqualTo(0))
const amount=Schema.Number.check(Schema.isFinite(),Schema.isGreaterThanOrEqualTo(0))
const timestamp=Schema.Number.check(Schema.isFinite())
const nonEmptyTrimmedString=Schema.NonEmptyString.check(Schema.isTrimmed())
const unknown=Schema.TaggedStruct('Unknown',{reason:Schema.Literals(['no-provider','not-covered','not-attributed','out-of-retention','loading','failed']),detail:Schema.optional(Schema.String)})
const fact=<S extends Schema.Top>(value:S)=>Schema.Union([Schema.TaggedStruct('Known',{value,source:Schema.Literals(['st','series']),asOfMs:Schema.optional(timestamp)}),unknown])
const cost=Schema.Struct({uncachedInputUsd:amount,cacheReadUsd:amount,cacheWriteUsd:amount,outputUsd:amount})
const spendScope=Schema.Union([Schema.TaggedStruct('Self',{}),Schema.TaggedStruct('IncludingSubagents',{}),Schema.TaggedStruct('AgentIncarnations',{}),Schema.TaggedStruct('Unknown',{reason:Schema.Literal('not-attributed')})])
const coverage=Schema.Union([Schema.TaggedStruct('Complete',{}),Schema.TaggedStruct('Partial',{attributedFraction:amount.check(Schema.isLessThanOrEqualTo(1))}),Schema.TaggedStruct('NotCovered',{harnessName:nonEmptyTrimmedString}),Schema.TaggedStruct('Unavailable',{reason:Schema.Literals(['no-provider','not-covered','not-attributed','out-of-retention','loading','failed'])})])
const accumulationWindow=Schema.Union([Schema.TaggedStruct('AllTime',{}),Schema.TaggedStruct('Interval',{fromMs:timestamp,toMs:timestamp}),Schema.TaggedStruct('Reported',{label:nonEmptyTrimmedString})])
const subagent=Schema.Struct({sessionId:Schema.String,parentSessionId:fact(Schema.String),label:fact(Schema.String),model:fact(Schema.String),usd:amount,requests:count,firstMs:fact(timestamp),lastMs:fact(timestamp)})
const buckets=Schema.Struct({range:Schema.Literals(['1d','7d','30d']),bucketMs:count,points:Schema.Array(Schema.Struct({tMs:timestamp,tokens:Schema.Struct({input:count,output:count,cacheRead:count,cacheWrite:count}),cost,model:Schema.String,contextTokens:Schema.NullOr(count),contextCapacity:Schema.NullOr(count),compactionEpoch:Schema.NullOr(count),subagentUsd:amount,requests:count})),compactions:Schema.Array(Schema.Struct({tMs:timestamp,sessionId:Schema.String})),coverage:Schema.Struct({harnesses:Schema.Array(Schema.Literals(['omp','claude','codex'])),requestsIncluded:count,requestsTotal:count,oldestMs:timestamp})})
const turns=Schema.Struct({points:Schema.Array(Schema.Struct({turnIndex:count,tMs:timestamp,contextTokens:Schema.NullOr(count),contextPercent:Schema.NullOr(amount.check(Schema.isLessThanOrEqualTo(100))),estimated:Schema.Boolean})),compactions:Schema.Array(Schema.Struct({tMs:timestamp,sessionId:Schema.String}))})
/** The single portable trace schema; providers decode this at their data boundary. */
export const TraceSeriesSchema=Schema.Struct({
 agentId:Schema.String,nativeSessionId:fact(Schema.String),
 meters:Schema.Struct({context:Schema.Struct({reportedPercent:fact(amount.check(Schema.isLessThanOrEqualTo(100))),usedTokens:fact(count),windowTokens:fact(count),compactions:fact(count),lastCompactionMs:fact(timestamp)}),spend:Schema.Struct({usdScope:spendScope,usdWindow:fact(accumulationWindow),totalUsdCoverage:coverage,mainUsdScope:spendScope,mainUsdWindow:fact(accumulationWindow),mainUsdCoverage:coverage,subagentUsdScope:spendScope,subagentUsdWindow:fact(accumulationWindow),subagentUsdCoverage:coverage,tokensScope:spendScope,totalUsd:fact(amount),mainUsd:fact(amount),subagentUsd:fact(amount),tokens:fact(count),tokensWindow:fact(accumulationWindow),split:fact(cost)})}),
 coverage,
 subagentCount:fact(count),
 subagentsWindow:fact(accumulationWindow),
 series:fact(buckets),turns:fact(turns),subagents:fact(Schema.Array(subagent)),history:fact(Schema.Array(Schema.Struct({nativeSessionId:Schema.String,startedMs:timestamp,endedMs:Schema.NullOr(timestamp),usd:amount,compactions:count}))),
})
type Equal<A,B>=(<T>()=>T extends A?1:2) extends (<T>()=>T extends B?1:2)?true:false
type Assert<T extends true>=T
export type TraceSeriesSchemaMatchesProjection=Assert<Equal<typeof TraceSeriesSchema.Type,TraceSeries>>
