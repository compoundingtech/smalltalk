import * as stylex from '@stylexjs/stylex'
import type { ReactNode } from 'react'
import { accentVars as accent, borderVars as border, geometryVars as geometry, radiusVars as radius, spaceVars as space, surfaceVars as surface, textVars as text, typeVars as type } from '../composition-tokens.stylex.ts'
import type { TraceSeries, TraceUnknownReason } from './trace-series.ts'
import { contextPercent, traceCoverageState, type TraceAccumulationWindow, type TraceCoverage, type TraceCoverageState, type TraceFact, type TraceSpendScope } from './trace-series.ts'
import { Button, Tooltip, TooltipTrigger } from 'react-aria-components'
import type { DetailsEmptyRule } from './DetailsResources.tsx'
import { compactNumber, money, clock } from './trace/chart.ts'
import { adaptTrace } from './trace/adapter.ts'
import { CompactSpend, CompactSpendPending } from './trace/CompactSpend.tsx'
import { ContextPanel, ContextPanelPending, contextPanelFootprint, type ContextPanelFootprint } from './trace/ContextPanel.tsx'
import { SpendPanel, SpendPanelPending } from './trace/SpendPanel.tsx'
import { DetailsUnavailable, MissingDetailsValue, unavailableSubject } from './DetailsUnavailable.tsx'
export interface DetailsChartsProps {
  readonly trace: TraceSeries
  readonly context: 'C1' | 'C2' | 'C3'
  readonly spend: 'S1' | 'S2' | 'S3'
  readonly empty?: DetailsEmptyRule
  /** Render an unwrapped body for a parent's independently collapsible section. */
  readonly section?: 'context' | 'spend'
  /** Suppress the section summary when its parent supplies the single unavailable line. */
  readonly showUnavailableReasons?: boolean
}

const missingReason = (fact: TraceFact<unknown>): string => fact._tag === 'Unknown' ? fact.detail ?? fact.reason : 'Context occupancy could not be derived from the reported values.'

const seriesExplanation: Record<TraceUnknownReason, string> = {
  'no-provider': 'The current source does not provide historical samples.',
  'not-covered': 'Historical samples are not available for this session.',
  'not-attributed': 'Historical samples are not attributed to this session.',
  'out-of-retention': 'Historical samples are no longer retained.',
  loading: 'Historical samples are still loading.',
  failed: 'Historical samples could not be loaded.',
}

const ContextMeter = ({ trace }: { readonly trace: TraceSeries }): ReactNode => {
  const percentage = contextPercent(trace.meters.context)
  const { compactions, lastCompactionMs } = trace.meters.context
  const occupancyReason = Object.values(trace.meters.context).filter(fact => fact._tag === 'Unknown').map(missingReason).join(' · ') || 'Context occupancy could not be derived from the reported values.'
  return <div {...stylex.props(styles.contextSnapshot)}>
    <div data-details-data-row={percentage !== undefined ? true : undefined} {...stylex.props(styles.numberRow)}>{percentage !== undefined ? <strong>{Math.round(percentage)}%</strong> : <span>Context occupancy <MissingDetailsValue label="Context occupancy" reason={occupancyReason} /></span>}</div>
    {percentage !== undefined && <div {...stylex.props(styles.track)} role="meter" aria-label="Context occupancy" aria-valuemin={0} aria-valuemax={100} aria-valuenow={Math.max(0, Math.min(100, percentage))} aria-valuetext={`${Math.round(percentage)} percent`}><div {...stylex.props(styles.fill(`${Math.max(0, Math.min(100, percentage))}%`))} /></div>}
    <div data-details-data-row={compactions._tag === 'Known' || lastCompactionMs._tag === 'Known' ? true : undefined} {...stylex.props(styles.numberRow)}><span>Compactions {compactions._tag === 'Known' ? compactNumber.format(compactions.value) : <MissingDetailsValue label="Compactions" reason={missingReason(compactions)} />}</span><span>Last {lastCompactionMs._tag === 'Known' ? `${clock.format(lastCompactionMs.value)} UTC` : <MissingDetailsValue label="Last compaction" reason={missingReason(lastCompactionMs)} />}</span></div>
  </div>
}

/** Loading twin of ContextMeter: formatted footprints wrap naturally under text-shaped ink. */
const ContextMeterPending = ({ footprint }: { readonly footprint: DetailsPendingShape['context']['meter'] }): ReactNode => <div {...stylex.props(styles.contextSnapshot)}>
  <div {...stylex.props(styles.numberRow)}><strong><span {...stylex.props(styles.pendingText)}>{footprint.percentage ?? 'Context occupancy —'}</span></strong></div>
  {footprint.percentage !== undefined && <div {...stylex.props(styles.track)} />}
  <div {...stylex.props(styles.numberRow)}><span><span {...stylex.props(styles.pendingText)}>{footprint.compactions ?? 'Compactions —'}</span></span><span><span {...stylex.props(styles.pendingText)}>{footprint.lastCompaction ?? 'Last —'}</span></span></div>
</div>


const scopeLabel = (scope:TraceSpendScope):string => scope._tag==='Self'?' · self':scope._tag==='IncludingSubagents'?' · including subagents':scope._tag==='AgentIncarnations'?' · agent incarnations':''
const spendScopeLabel = (trace:TraceSeries):string => scopeLabel(trace.meters.spend.usdScope)
export const accumulationParts = (fact:TraceFact<TraceAccumulationWindow>):readonly string[]|undefined => {
  if(fact._tag==='Unknown')return undefined
  const window=fact.value
  if(window._tag==='AllTime')return ['all time']
  if(window._tag==='Reported')return [`reported window · ${window.label}`]
  const from = new Date(window.fromMs).toISOString()
  const to = new Date(window.toMs).toISOString()
  return from.slice(0,10) === to.slice(0,10)
    ? [`${from.slice(0,10)} ${from.slice(11,16)}–${to.slice(11,16)} UTC`]
    : [`${from.slice(0,10)} ${from.slice(11,16)}–`, `${to.slice(0,10)} ${to.slice(11,16)} UTC`]
}
const tokenScopeLabel = (trace:TraceSeries):{readonly label:string;readonly window?:readonly string[]|undefined} => ({
 label:`Cumulative tokens${trace.meters.spend.tokensScope._tag==='Self'?' · this agent':trace.meters.spend.tokensScope._tag==='IncludingSubagents'?' · this agent + subagents':trace.meters.spend.tokensScope._tag==='AgentIncarnations'?' · observed agent incarnations':''}`,
 window:accumulationParts(trace.meters.spend.tokensWindow),
})
const totalLabel=(trace:TraceSeries):string=>`${trace.meters.spend.totalUsdCoverage._tag==='Complete'?'Total':'Recorded spend'}${spendScopeLabel(trace)}`
/** Both row props and pending footprints normalize only the malformed row's coverage. */
const coverageLabel=(coverage:TraceCoverageState):string|undefined=>{
 const state=traceCoverageState(coverage)
 return state._tag==='Complete'?undefined:state._tag==='PartialUnreported'?'Partial':state._tag==='Partial'?`Partial · ${Math.min(99,Math.floor(state.attributedFraction*100))}% attributed`:state._tag==='NotCovered'?`Not tracked for ${state.harnessName}`:'Attribution —'
}
export const TraceCoverageNote = ({trace,coverage=trace.coverage}:{readonly trace:TraceSeries;readonly coverage?:TraceCoverage}):ReactNode => {
 const state=traceCoverageState(coverage)
 if(state._tag==='Complete')return null
 if(state._tag==='Unavailable')return <span {...stylex.props(styles.tokenWindow)}>Attribution <MissingDetailsValue label="Attribution coverage" reason={state.reason}/></span>
 if(state._tag==='NotCovered')return <TooltipTrigger delay={0}><Button data-details-coverage {...stylex.props(styles.coverageTrigger)}>{coverageLabel(state)}</Button><Tooltip {...stylex.props(styles.coverageTooltip)}>Spend is recorded only for OMP sessions today.</Tooltip></TooltipTrigger>
 return <p data-details-coverage {...stylex.props(styles.scopeNote)}>{coverageLabel(state)}</p>
}

/** Both live and pending rows use the same reported window and wrapping boundaries. */
const MetricWindow = ({ parts, pending = false, tokens = false }: { readonly parts: readonly string[]; readonly pending?: boolean;readonly tokens?:boolean }): ReactNode =>
  <span data-details-metric-period={pending?undefined:true} data-details-token-period={pending||!tokens ? undefined : true} {...stylex.props(styles.tokenWindow)}>{parts.map((part,index)=><span key={index}>{index>0?<wbr/>:null}<span {...stylex.props(styles.windowPart)}><span {...stylex.props(pending&&styles.pendingText)}>{part}</span></span></span>)}</span>

const UsdRow = ({trace,metric,label,fact,scope,window,coverage}:{readonly trace:TraceSeries;readonly metric:string;readonly label:string;readonly fact:TraceFact<number>;readonly scope:TraceSpendScope;readonly window:TraceFact<TraceAccumulationWindow>;readonly coverage:TraceCoverage}):ReactNode => {
 const parts=accumulationParts(window)
 return <div data-details-metric={metric} {...stylex.props(styles.spendRow)}><dt {...stylex.props(styles.label)}>{label}{scopeLabel(scope)}{scope._tag==='Unknown'&&<MissingDetailsValue label={`${label} scope`} reason={scope.reason}/>} {parts!==undefined?<MetricWindow parts={parts}/>:<span {...stylex.props(styles.tokenWindow)}>Window <MissingDetailsValue label={`${label} accumulation window`} reason={missingReason(window)}/></span>}<TraceCoverageNote trace={trace} coverage={coverage}/></dt><dd {...stylex.props(styles.amount)}>{fact._tag==='Known'?money.format(fact.value):<MissingDetailsValue label={label} reason={missingReason(fact)}/>}</dd></div>
}
const SpendMeter = ({ trace }: { readonly trace: TraceSeries }): ReactNode => {
  const spend = trace.meters.spend
  const tokenLabel = tokenScopeLabel(trace)
  return <div {...stylex.props(styles.body)}><dl {...stylex.props(styles.spendSnapshot)} aria-label="Trace spend meters">
    <UsdRow trace={trace} metric="total-usd" label={spend.totalUsdCoverage._tag==='Complete'?'Total':'Recorded spend'} fact={spend.totalUsd} scope={spend.usdScope} window={spend.usdWindow} coverage={spend.totalUsdCoverage}/>
    <UsdRow trace={trace} metric="main-usd" label="Main session" fact={spend.mainUsd} scope={spend.mainUsdScope} window={spend.mainUsdWindow} coverage={spend.mainUsdCoverage}/>
    <UsdRow trace={trace} metric="subagent-usd" label={spend.subagentUsdCoverage._tag==='Partial'?'Attributed subagents':'Subagents'} fact={spend.subagentUsd} scope={spend.subagentUsdScope} window={spend.subagentUsdWindow} coverage={spend.subagentUsdCoverage}/>
    <div {...stylex.props(styles.spendRow)}><dt {...stylex.props(styles.label)}><span data-details-token-label>{tokenLabel.label}</span>{spend.tokensScope._tag==='Unknown'&&<MissingDetailsValue label="Cumulative tokens scope" reason={spend.tokensScope.reason}/>}<span {...stylex.props(styles.tokenWindow)}>{tokenLabel.window!==undefined?<MetricWindow parts={tokenLabel.window} tokens/>:<>Window <MissingDetailsValue label="Cumulative tokens accumulation window" reason={missingReason(spend.tokensWindow)}/></>}</span></dt><dd {...stylex.props(styles.tokens)}>{spend.tokens._tag==='Known'?compactNumber.format(spend.tokens.value):<MissingDetailsValue label="Cumulative tokens" reason={missingReason(spend.tokens)}/>}</dd></div>
  </dl>
  <TraceCoverageNote trace={trace}/>
  {trace.subagents._tag==='Known'&&trace.subagents.value.length>0&&<p data-details-subagent-spend-window {...stylex.props(styles.scopeNote)}>Traced sessions · {accumulationParts(trace.subagentsWindow)?.join('')??<MissingDetailsValue label="Traced sessions accumulation window" reason={missingReason(trace.subagentsWindow)}/>}</p>}
  {trace.subagents._tag === 'Known' && trace.subagents.value.length > 0 && <dl {...stylex.props(styles.spendSnapshot)} aria-label="Subagent spend breakdown">{trace.subagents.value.map((child) => <div key={child.sessionId} {...stylex.props(styles.spendRow)}><dt {...stylex.props(styles.label)}>{child.label._tag==='Known'?child.label.value:child.sessionId}</dt><dd {...stylex.props(styles.amount)}>{money.format(child.usd)}</dd></div>)}</dl>}</div>
}

/** Loading twin of SpendMeter for the populated (attributed, subagent-bearing) shape. */
const SpendMeterPending = ({ footprint }: { readonly footprint: DetailsPendingShape['spend'] }): ReactNode => <div aria-hidden="true" inert {...stylex.props(styles.body)}>
  <dl {...stylex.props(styles.spendSnapshot)}>
    {footprint.rows.map(row=><div key={row.label} {...stylex.props(styles.spendRow)}><dt {...stylex.props(styles.label)}><span {...stylex.props(styles.pendingText)}>{row.label}</span>{row.window!==undefined&&<MetricWindow parts={row.window} pending tokens={row.tokens}/>} {row.coverage!==undefined&&<p {...stylex.props(styles.scopeNote)}><span {...stylex.props(styles.pendingText)}>{row.coverage}</span></p>}</dt><dd {...stylex.props(row.tokens?styles.tokens:styles.amount)}><span {...stylex.props(styles.pendingText)}>{row.value}</span></dd></div>)}
  </dl>
  {footprint.coverage!==undefined&&<p {...stylex.props(styles.scopeNote)}><span {...stylex.props(styles.pendingText)}>{footprint.coverage}</span></p>}
  {footprint.subagents.length>0&&footprint.subagentWindow!==undefined&&<p {...stylex.props(styles.scopeNote)}><span {...stylex.props(styles.pendingText)}>{footprint.subagentWindow}</span></p>}
  {footprint.subagents.length>0&&<dl {...stylex.props(styles.spendSnapshot)}>{footprint.subagents.map((row,index)=><div key={index} {...stylex.props(styles.spendRow)}><dt {...stylex.props(styles.label)}><span {...stylex.props(styles.pendingText)}>{row.label}</span></dt><dd {...stylex.props(styles.amount)}><span {...stylex.props(styles.pendingText)}>{row.value}</span></dd></div>)}</dl>}
</div>

const hasSpendMeterData = (trace: TraceSeries): boolean => {
  const spend = trace.meters.spend
  return spend.totalUsd._tag === 'Known' || spend.tokens._tag === 'Known'
    || spend.mainUsd._tag === 'Known' || spend.subagentUsd._tag === 'Known'
    || (trace.subagents._tag === 'Known' && trace.subagents.value.length > 0)
}

const hasReportedSplit = (trace: TraceSeries): boolean => trace.meters.spend.mainUsdScope._tag !== 'Unknown'
  && trace.meters.spend.split._tag === 'Known' && Object.values(trace.meters.spend.split.value).every(Number.isFinite)

/** Match the selected chart's usable rows, not merely the existence of a provider. */
export const hasDetailsChartData = ({ trace, context, spend, section, empty = 'E1' }: DetailsChartsProps): boolean => {
  const projection = adaptTrace(trace)
  const samples = projection?.lane.turnSamples !== undefined && (context === 'C2' || projection.lane.samples.length === 0) ? projection.lane.turnSamples : projection?.lane.samples
  const contextData = contextPercent(trace.meters.context)!==undefined || trace.meters.context.compactions._tag==='Known' || trace.meters.context.lastCompactionMs._tag==='Known'
    || (context !== 'C1' && samples?.some(sample => sample.contextPercent !== null) === true)
  if (section === 'context') return contextData
  const meters = trace.meters.spend
  let spendData: boolean
  if (empty === 'E3') {
    spendData = hasSpendMeterData(trace) || (meters.mainUsdScope._tag !== 'Unknown' && projection !== undefined && spend !== 'S1'
      && (projection.lane.cumulativeValues.length > 0 || (spend === 'S2' && projection.descendantSpend.cumulativeValues.length > 0)))
      || (spend === 'S3' && hasReportedSplit(trace))
  } else if (spend === 'S1' || meters.mainUsdScope._tag === 'Unknown' || projection === undefined) {
    spendData = hasSpendMeterData(trace)
  } else {
    spendData = projection.lane.totalUsd._tag === 'Known' || (spend === 'S2'
      ? projection.descendantSpend.totalUsd._tag === 'Known' || projection.lane.cumulativeValues.length > 0
      : meters.split._tag === 'Known' && projection.lane.samples.length > 0)
  }
  spendData ||= hasSpendMeterData(trace)
  return section === 'spend' ? spendData : contextData || spendData
}

type ChartGap = { readonly subject: string; readonly reason: string }
const contextFactNames: Record<keyof TraceSeries['meters']['context'], string> = {
  reportedPercent: 'Reported percentage', usedTokens: 'Used tokens', windowTokens: 'Context window', compactions: 'Compactions', lastCompactionMs: 'Last compaction',
}
/** Each unreported fact keeps the name the section's single unavailable line paints. */
const sectionGaps = (trace: TraceSeries, section: 'context' | 'spend'): readonly ChartGap[] => {
  if (section === 'context') return (Object.keys(contextFactNames) as (keyof typeof contextFactNames)[]).flatMap(key => trace.meters.context[key]._tag === 'Unknown' ? [{ subject: contextFactNames[key], reason: missingReason(trace.meters.context[key]) }] : [])
  const spend = trace.meters.spend
  const facts = [['Total spend', spend.totalUsd], ['Main session spend', spend.mainUsd], ['Subagent spend', spend.subagentUsd], ['Cumulative tokens', spend.tokens], ['Accumulation windows', spend.usdWindow], ['Accumulation windows', spend.mainUsdWindow], ['Accumulation windows', spend.subagentUsdWindow], ['Accumulation windows', spend.tokensWindow]] as const
  const scopes = [spend.usdScope, spend.mainUsdScope, spend.subagentUsdScope, spend.tokensScope]
  const coverage = [trace.coverage, spend.totalUsdCoverage, spend.mainUsdCoverage, spend.subagentUsdCoverage]
  return [
    ...facts.flatMap(([subject, fact]) => fact._tag === 'Unknown' ? [{ subject, reason: missingReason(fact) }] : []),
    ...scopes.flatMap(scope => scope._tag === 'Unknown' ? [{ subject: 'Spend scope', reason: scope.reason }] : []),
    ...coverage.flatMap(state => state._tag === 'Unavailable' ? [{ subject: 'Attribution', reason: state.reason }] : []),
    ...(trace.subagents._tag === 'Known' && trace.subagents.value.length > 0 && trace.subagentsWindow._tag === 'Unknown' ? [{ subject: 'Traced-session window', reason: missingReason(trace.subagentsWindow) }] : []),
  ]
}
const GapSummary = ({ gaps }: { readonly gaps: readonly ChartGap[] }): ReactNode => gaps.length === 0 ? null
  : <DetailsUnavailable subjects={gaps.map(gap => gap.subject)} reason={[...new Set(gaps.map(gap => gap.reason))].join(' · ')} />

/** Meters and drawing evidence share one portable Known/Unknown contract. */
export const DetailsCharts = ({ trace, context, spend, section, showUnavailableReasons = true }: DetailsChartsProps): ReactNode => {
  const projection = adaptTrace(trace)
  const seriesReason = trace.series._tag === 'Unknown' ? trace.series.detail ?? seriesExplanation[trace.series.reason] : 'No historical samples were retained.'
  const spendProjection = trace.meters.spend.mainUsdScope._tag === 'Unknown' ? undefined : projection
  const spendReason = trace.meters.spend.mainUsdScope._tag === 'Unknown' ? 'Session spend scope is not attributed' : seriesReason
  const contextSamples = projection?.lane.turnSamples !== undefined && (context === 'C2' || projection.lane.samples.length === 0) ? projection.lane.turnSamples : projection?.lane.samples
  const contextSeries = context !== 'C1' && contextSamples?.some(sample => sample.contextPercent !== null) === true
  const compactSeries = spend === 'S2' && spendProjection !== undefined && (spendProjection.lane.cumulativeValues.length > 0 || spendProjection.descendantSpend.cumulativeValues.length > 0)
  const panelSeries = spend === 'S3' && spendProjection !== undefined && spendProjection.lane.cumulativeValues.length > 0
  const contextGaps = [...sectionGaps(trace, 'context'), ...(context !== 'C1' && !contextSeries ? [{ subject: 'Context series', reason: seriesReason }] : [])]
  const spendGaps = [...sectionGaps(trace, 'spend'), ...(spend !== 'S1' && !compactSeries && !panelSeries ? [{ subject: 'Spend series', reason: spendReason }] : []), ...(compactSeries && spendProjection !== undefined && (spendProjection.lane.cumulativeValues.length === 0 || spendProjection.descendantSpend.cumulativeValues.length === 0) ? [{ subject: 'Spend series', reason: 'Session or descendant spend series was not retained.' }] : []), ...(spend === 'S3' && !hasReportedSplit(trace) ? [{ subject: 'Spend split', reason: 'Attributed component samples were not supplied.' }] : [])]
  const contextBody = <div {...stylex.props(styles.body)} data-details-chart={context}>
    <ContextMeter trace={trace} />
    {context !== 'C1' && (contextSeries && projection !== undefined ? <ContextPanel scene={projection.scene} lane={projection.lane} compact={context === 'C2'} /> : <span {...stylex.props(styles.label)}>Context series <MissingDetailsValue label="Context series" reason={seriesReason}/></span>)}
    {showUnavailableReasons && <GapSummary gaps={contextGaps} />}
  </div>
  const spendBody = <div {...stylex.props(styles.body)} data-details-chart={spend}>
    <SpendMeter trace={trace} />
    {compactSeries && <CompactSpend lane={spendProjection!.lane} descendantSpend={spendProjection!.descendantSpend} showTotals={false}/>}
    {panelSeries && <SpendPanel lane={spendProjection!.lane} {...(hasReportedSplit(trace) && trace.meters.spend.split._tag === 'Known' ? {split:trace.meters.spend.split.value} : {})} componentsKnown={hasReportedSplit(trace)} showTotals={false}/>}
    {spend !== 'S1' && !compactSeries && !panelSeries && !(spend === 'S3' && hasReportedSplit(trace)) && <span {...stylex.props(styles.label)}>Spend series <MissingDetailsValue label="Spend series" reason={spendReason}/></span>}
    {spend === 'S3' && !panelSeries && !hasReportedSplit(trace) && <span {...stylex.props(styles.label)}>Spend split <MissingDetailsValue label="Four-way spend split" reason={trace.meters.spend.split._tag === 'Unknown' ? missingReason(trace.meters.spend.split) : 'Valid session-attributed spend components were not supplied.'}/></span>}
    {spend === 'S3' && !panelSeries && hasReportedSplit(trace) && trace.meters.spend.split._tag === 'Known' && <SpendPanel split={trace.meters.spend.split.value} componentsKnown showTotals={false}/>}
    {showUnavailableReasons && <GapSummary gaps={spendGaps} />}
  </div>
  if (section === 'context') return contextBody
  if (section === 'spend') return spendBody
  return <div {...stylex.props(styles.root)}><section aria-label="Context"><h3 {...stylex.props(styles.heading)}>Context</h3>{contextBody}</section><section aria-label="Spend"><h3 {...stylex.props(styles.heading)}>Spend</h3>{spendBody}</section></div>
}

/** Only formatted wrap footprints and structural counts; no pending observations are displayed. */
export interface DetailsPendingShape {
  readonly context: {
    readonly meter: {
      readonly percentage: string | undefined
      readonly compactions: string | undefined
      readonly lastCompaction: string | undefined
    }
    readonly compactPanel: ContextPanelFootprint | undefined
    readonly fullPanel: ContextPanelFootprint | undefined
    readonly gaps: readonly string[]
  }
  readonly spend: {
    readonly rows: readonly { readonly label: string; readonly window?: readonly string[]|undefined; readonly coverage?:string|undefined; readonly value: string; readonly tokens: boolean }[]
    readonly subagents: readonly { readonly label: string; readonly value: string }[]
    readonly subagentWindow?:string|undefined
    readonly gaps: readonly string[]
    readonly coverage?: string | undefined
    readonly compactChart?: { readonly series: boolean; readonly descendants: boolean; readonly partial: boolean }
    readonly fullChart?: { readonly series: boolean; readonly split: boolean; readonly partial: boolean }
  }
}

export const pendingShapeFromTrace = (trace: TraceSeries): DetailsPendingShape => {
  const projection = adaptTrace(trace)
  const { compactions, lastCompactionMs } = trace.meters.context
  const percentage = contextPercent(trace.meters.context)
  const spendProjection = trace.meters.spend.mainUsdScope._tag === 'Unknown' ? undefined : projection
  const retainedUsd = spendProjection?.lane.cumulativeValues.at(-1)?.usd ?? 0
  const retainedDescendants = spendProjection?.descendantSpend.cumulativeValues.at(-1)?.usd ?? 0
  const mainPartial = spendProjection?.lane.totalUsd._tag === 'Known' && Math.abs(retainedUsd - spendProjection.lane.totalUsd.value) > 0.005
  const descendantPartial = spendProjection?.descendantSpend.totalUsd._tag === 'Known' && Math.abs(retainedDescendants - spendProjection.descendantSpend.totalUsd.value) > 0.005
  return {
    context: {
      meter: {
        percentage: percentage !== undefined ? `${Math.round(percentage)}%` : undefined,
        compactions: compactions._tag === 'Known' ? `Compactions ${compactNumber.format(compactions.value)}` : undefined,
        lastCompaction: lastCompactionMs._tag === 'Known' ? `Last ${clock.format(lastCompactionMs.value)} UTC` : undefined,
      },
      compactPanel: projection !== undefined ? contextPanelFootprint(projection.lane, true) : undefined,
      fullPanel: projection !== undefined ? contextPanelFootprint(projection.lane, false) : undefined,
      gaps: sectionGaps(trace, 'context').map(gap => gap.subject),
    },
    spend: {
      rows: [
        {label:totalLabel(trace)+(trace.meters.spend.usdScope._tag==='Unknown'?' —':''),window:accumulationParts(trace.meters.spend.usdWindow)??['Window —'],coverage:coverageLabel(trace.meters.spend.totalUsdCoverage),value:trace.meters.spend.totalUsd._tag==='Known'?money.format(trace.meters.spend.totalUsd.value):'—',tokens:false},
        {label:`Main session${scopeLabel(trace.meters.spend.mainUsdScope)}${trace.meters.spend.mainUsdScope._tag==='Unknown'?' —':''}`,window:accumulationParts(trace.meters.spend.mainUsdWindow)??['Window —'],coverage:coverageLabel(trace.meters.spend.mainUsdCoverage),value:trace.meters.spend.mainUsd._tag==='Known'?money.format(trace.meters.spend.mainUsd.value):'—',tokens:false},
        {label:`${trace.meters.spend.subagentUsdCoverage._tag==='Partial'?'Attributed subagents':'Subagents'}${scopeLabel(trace.meters.spend.subagentUsdScope)}${trace.meters.spend.subagentUsdScope._tag==='Unknown'?' —':''}`,window:accumulationParts(trace.meters.spend.subagentUsdWindow)??['Window —'],coverage:coverageLabel(trace.meters.spend.subagentUsdCoverage),value:trace.meters.spend.subagentUsd._tag==='Known'?money.format(trace.meters.spend.subagentUsd.value):'—',tokens:false},
        {label:tokenScopeLabel(trace).label+(trace.meters.spend.tokensScope._tag==='Unknown'?' —':''),window:accumulationParts(trace.meters.spend.tokensWindow)??['Window —'],value:trace.meters.spend.tokens._tag==='Known'?compactNumber.format(trace.meters.spend.tokens.value):'—',tokens:true},
      ],
      subagents: trace.subagents._tag==='Known'?trace.subagents.value.map(child=>({label:child.label._tag==='Known'?child.label.value:child.sessionId,value:money.format(child.usd)})):[],
      subagentWindow:`Traced sessions · ${accumulationParts(trace.subagentsWindow)?.join('')??'—'}`,
      gaps: sectionGaps(trace, 'spend').map(gap => gap.subject),
      coverage: coverageLabel(trace.coverage),
      compactChart: { series: (spendProjection?.lane.cumulativeValues.length ?? 0) > 0, descendants: (spendProjection?.descendantSpend.cumulativeValues.length ?? 0) > 0, partial: mainPartial || descendantPartial },
      fullChart: { series: (spendProjection?.lane.cumulativeValues.length ?? 0) > 0, split: hasReportedSplit(trace), partial: mainPartial },
    },
  }
}

/**
 * Loading bodies for the pane's collapsible Context and Spend sections. They
 * mirror the populated per-variant structure (wrappers, gaps, chart aspect
 * ratios, row counts) so arrival cannot shift either section; the pane's one
 * SyncLine connection status is the only announced loading text.
 */
export const DetailsChartsPending = ({ context, spend, section, shape }: {
  readonly context: 'C1' | 'C2' | 'C3'
  readonly spend: 'S1' | 'S2' | 'S3'
  readonly section?: 'context' | 'spend'
  readonly shape: DetailsPendingShape
  /** Empty policy does not alter the reported meter geometry. */
  readonly empty?: DetailsEmptyRule
}): ReactNode => {
  const panelFootprint = context === 'C2' ? shape.context.compactPanel : shape.context.fullPanel
  const contextGaps = [...shape.context.gaps, ...(context !== 'C1' && panelFootprint === undefined ? ['Context series'] : [])]
  const spendGaps = [...shape.spend.gaps,
    ...((spend === 'S2' && shape.spend.compactChart !== undefined && (!shape.spend.compactChart.series || !shape.spend.compactChart.descendants)) || (spend === 'S3' && shape.spend.fullChart?.series === false) ? ['Spend series'] : []),
    ...(spend === 'S3' && shape.spend.fullChart?.split === false ? ['Spend split'] : [])]
  const contextBody = <div aria-hidden="true" inert data-details-skeleton="context" data-details-chart={context} {...stylex.props(styles.body)}>
    <ContextMeterPending footprint={shape.context.meter} />
    {context !== 'C1' && panelFootprint !== undefined && <ContextPanelPending compact={context === 'C2'} footprint={panelFootprint} />}
    {context !== 'C1' && panelFootprint === undefined && <span {...stylex.props(styles.label)}><span {...stylex.props(styles.pendingText)}>Context series —</span></span>}
    {contextGaps.length > 0 && <p {...stylex.props(styles.unknown)}><span {...stylex.props(styles.pendingText)}>{unavailableSubject(contextGaps)} · Unavailable</span></p>}
  </div>
  const spendBody = <div aria-hidden="true" data-details-skeleton="spend" data-details-chart={spend} {...stylex.props(styles.body)}>
    <SpendMeterPending footprint={shape.spend} />
    {spend === 'S2' && (shape.spend.compactChart === undefined || shape.spend.compactChart.series || shape.spend.compactChart.descendants ? <CompactSpendPending showTotals={false} {...(shape.spend.compactChart === undefined ? {} : {footprint:shape.spend.compactChart})}/> : <span {...stylex.props(styles.label)}><span {...stylex.props(styles.pendingText)}>Spend series —</span></span>)}
    {spend === 'S3' && shape.spend.fullChart?.series === false && !shape.spend.fullChart.split && <span {...stylex.props(styles.label)}><span {...stylex.props(styles.pendingText)}>Spend series —</span></span>}
    {spend === 'S3' && shape.spend.fullChart?.series === false && !shape.spend.fullChart.split && <span {...stylex.props(styles.label)}><span {...stylex.props(styles.pendingText)}>Spend split —</span></span>}
    {spend === 'S3' && (shape.spend.fullChart === undefined || shape.spend.fullChart.series || shape.spend.fullChart.split) && <SpendPanelPending showTotals={false} {...(shape.spend.fullChart === undefined ? {} : {footprint:shape.spend.fullChart})}/>}
    {spendGaps.length > 0 && <p {...stylex.props(styles.unknown)}><span {...stylex.props(styles.pendingText)}>{unavailableSubject(spendGaps)} · Unavailable</span></p>}
  </div>
  if (section === 'context') return contextBody
  if (section === 'spend') return spendBody
  return <div {...stylex.props(styles.root)} data-details-skeleton><section aria-label="Context"><h3 {...stylex.props(styles.heading)}>Context</h3>{contextBody}</section><section aria-label="Spend"><h3 {...stylex.props(styles.heading)}>Spend</h3>{spendBody}</section></div>
}

const styles = stylex.create({
  root: { display: 'flex', flexDirection: 'column', gap: space.lg, minWidth: 0 },
  body: { display: 'flex', flexDirection: 'column', gap: space.md, minWidth: 0, color: text.fg, fontSize: type.metaSize, lineHeight: type.metaLeading },
  heading: { margin: 0, marginBottom: space.md, fontSize: type.metaSize, lineHeight: type.metaLeading, fontWeight: type.weightMedium, color: text.fg },
  unknown: { margin: 0, color: text.fgSoft, fontSize: type.denseSize, lineHeight: type.metaLeading },
  scopeNote: { margin: 0, color: text.fgSoft, fontSize: type.denseSize, lineHeight: type.metaLeading },
  coverageTrigger: { appearance:'none', borderWidth:0, padding:0, backgroundColor:surface.transparent, textAlign:'left', color:text.fgSoft, fontSize:type.denseSize, cursor:'help', outlineColor:accent.primary, outlineOffset:2 },
  coverageTooltip: { backgroundColor:surface.canvas, color:text.fg, borderWidth:1, borderStyle:'solid', borderColor:border.borderStrong, padding:space.md, fontSize:type.metaSize, maxWidth:280 },
  contextSnapshot: { display: 'flex', flexDirection: 'column', gap: space.xs, minWidth: 0 },
  numberRow: { display: 'flex', justifyContent: 'space-between', flexWrap: 'wrap', gap: space.md, fontVariantNumeric: 'tabular-nums' },
  track: { height: geometry.progressMd, backgroundColor: surface.rowActive, borderRadius: radius.full, overflow: 'hidden' },
  fill: (width: string) => ({ height: '100%', width, backgroundColor: accent.primary, borderRadius: radius.full }),
  spendSnapshot: { display: 'flex', flexDirection: 'column', gap: space.xs, margin: 0, minWidth: 0 },
  spendRow: { display: 'grid', gridTemplateColumns: `var(--details-spend-columns, minmax(0, 1fr) minmax(${geometry.detailsValueMin}, max-content))`, alignItems: 'baseline', gap: space.md, fontVariantNumeric: 'tabular-nums' },
  label: { minWidth: 0, color: text.fgMuted, overflowWrap: 'normal' },
  tokenWindow: { display: 'block', minWidth: 0, color: text.fgSoft, fontSize: type.denseSize, lineHeight: type.metaLeading },
  // Reported window descriptions wrap between words at narrow widths without shortening scope or provenance; words never break.
  windowPart: { whiteSpace: 'normal', overflowWrap: 'normal' },
  pendingText: { color: surface.transparent, backgroundImage: `linear-gradient(${border.borderStrong}, ${border.borderStrong})`, backgroundSize: `100% ${space.md}`, backgroundPosition: 'center', backgroundRepeat: 'no-repeat', boxDecorationBreak: 'clone' },
  amount: { margin: 0, textAlign: 'var(--details-value-align, right)', overflowWrap: 'normal' },
  tokens: { margin: 0, color: text.fgMuted, textAlign: 'var(--details-value-align, right)', overflowWrap: 'normal' },
})
