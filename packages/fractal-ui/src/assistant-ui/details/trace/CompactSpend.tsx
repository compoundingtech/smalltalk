import * as stylex from '@stylexjs/stylex'
import type { ReactNode } from 'react'
import { accentVars as accent, borderVars as border, spaceVars as space, textVars as text, typeVars as type } from '../../composition-tokens.stylex.ts'
import { clock, money } from './chart.ts'
import type { ChartLane as Lane, TraceChartProjection } from './adapter.ts'
import { DetailsInk, DetailsSkeletonPlot } from '../DetailsSkeleton.tsx'
import { MissingDetailsValue } from '../DetailsUnavailable.tsx'
export interface CompactSpendProps {
  readonly lane: Lane
  /** Role aggregates when the provider cannot attribute marks to individual descendants. */
  readonly descendantSpend: TraceChartProjection['descendantSpend']
  readonly showTotals?: boolean
}

/** Ported CompactSpend: separate self / full-subtree cumulative costs and honest retained marks. */
export const CompactSpend = ({ lane, descendantSpend, showTotals = true }: CompactSpendProps): ReactNode => {
  const descendantUsd = descendantSpend.totalUsd
  const descendantSeries = [descendantSpend.cumulativeValues]
  const times = [...new Set([...lane.cumulativeValues.map((point) => point.ts), ...descendantSpend.cumulativeValues.map((point) => point.ts)])].toSorted((a, b) => a - b)
  const start = times[0] ?? 0
  const end = times.at(-1) ?? start
  const maximum = Math.max(0.000001, lane.cumulativeValues.at(-1)?.usd ?? 0, descendantSpend.cumulativeValues.at(-1)?.usd ?? 0)
  const path = (series: ReadonlyArray<ReadonlyArray<{ readonly ts: number; readonly usd: number }>>): string => {
    return series[0]!.map((point, index) => {
      const x = end === start ? 160 : 4 + ((point.ts - start) / (end - start)) * 312
      const y = 76 - point.usd / maximum * 72
      return `${index === 0 ? 'M' : 'L'}${x.toFixed(1)},${y.toFixed(1)}`
    }).join(' ')
  }
  const retainedRoot = lane.cumulativeValues.at(-1)?.usd ?? 0
  const retainedDescendants = descendantSpend.cumulativeValues.at(-1)?.usd ?? 0
  const partial = (lane.totalUsd._tag === 'Known' && Math.abs(retainedRoot - lane.totalUsd.value) > 0.005) || (descendantUsd._tag === 'Known' && Math.abs(retainedDescendants - descendantUsd.value) > 0.005)
  const hasSeries = lane.cumulativeValues.length > 0
  const hasDescendantSeries = descendantSpend.cumulativeValues.length > 0
  return <div {...stylex.props(styles.root)} aria-label="Session and descendant spend">
    {showTotals && <div data-details-data-row={lane.totalUsd._tag === 'Known' || descendantUsd._tag === 'Known' ? true : undefined} {...stylex.props(styles.totals)}><span {...stylex.props(styles.session)}>Main session <strong>{lane.totalUsd._tag === 'Known' ? money.format(lane.totalUsd.value) : <MissingDetailsValue label="Main session spend" reason={lane.totalUsd.detail ?? lane.totalUsd.reason}/>}</strong></span><span {...stylex.props(styles.descendants)}>Subagents <strong>{descendantUsd._tag === 'Known' ? money.format(descendantUsd.value) : <MissingDetailsValue label="Subagent spend" reason={descendantUsd.detail ?? descendantUsd.reason}/>}</strong></span></div>}
    {hasSeries || hasDescendantSeries ? <svg {...stylex.props(styles.chart)} viewBox="0 0 320 80" preserveAspectRatio="none" role="img" aria-label="Cumulative session and descendant API-equivalent spend">
      {hasSeries && <path d={path([lane.cumulativeValues])} fill="none" stroke={accent.primary} strokeWidth={1.5} vectorEffect="non-scaling-stroke" />}
      {hasDescendantSeries && <path d={path(descendantSeries)} fill="none" stroke={text.fgSoft} strokeWidth={1.5} strokeDasharray="4 2" vectorEffect="non-scaling-stroke" />}
    </svg> : <span {...stylex.props(styles.note)}>Spend series <MissingDetailsValue label="Spend series" reason="Session and descendant spend series were not retained."/></span>}
    {hasSeries && !hasDescendantSeries && <span {...stylex.props(styles.note)}>Subagent series <MissingDetailsValue label="Subagent spend series" reason="Descendant spend series was not retained."/></span>}
    {!hasSeries && hasDescendantSeries && <span {...stylex.props(styles.note)}>Main session series <MissingDetailsValue label="Main session spend series" reason="Session spend series was not retained."/></span>}
    {(hasSeries || hasDescendantSeries) && <div {...stylex.props(styles.axis)}><span>{clock.format(start)}</span><span>UTC</span><span>{clock.format(end)}</span></div>}
    <p {...stylex.props(styles.note)}>API-equivalent · retained role observations{partial ? ' · chart and reported accumulation windows differ' : ''}</p>
</div>
}

/** Loading twin of CompactSpend: same wrappers, gaps and 4:1 chart box, ink instead of values. */
export const CompactSpendPending = ({showTotals=true,footprint={series:true,descendants:true,partial:false}}:{readonly showTotals?:boolean;readonly footprint?:{readonly series:boolean;readonly descendants:boolean;readonly partial:boolean}}): ReactNode => <div aria-hidden="true" inert {...stylex.props(styles.root)}>
  {showTotals&&<div {...stylex.props(styles.totals)}><span {...stylex.props(styles.session)}><span {...stylex.props(styles.pendingText)}>Main session <strong>$0.00</strong></span></span><span {...stylex.props(styles.descendants)}><span {...stylex.props(styles.pendingText)}>Subagents <strong>$0.00</strong></span></span></div>}
  {footprint.series || footprint.descendants ? <DetailsSkeletonPlot spend /> : <span {...stylex.props(styles.note)}><span {...stylex.props(styles.pendingText)}>Spend series —</span></span>}
  {footprint.series && !footprint.descendants && <span {...stylex.props(styles.note)}><span {...stylex.props(styles.pendingText)}>Subagent series —</span></span>}
  {!footprint.series && footprint.descendants && <span {...stylex.props(styles.note)}><span {...stylex.props(styles.pendingText)}>Main session series —</span></span>}
  {(footprint.series || footprint.descendants) && <div {...stylex.props(styles.axis)}><span><DetailsInk ch={5} /></span><span><span {...stylex.props(styles.pendingText)}>UTC</span></span><span><DetailsInk ch={5} /></span></div>}
  <p {...stylex.props(styles.note)}><span {...stylex.props(styles.pendingText)}>API-equivalent · retained role observations{footprint.partial ? ' · chart and reported accumulation windows differ' : ''}</span></p>
</div>


const styles = stylex.create({
  root: { display: 'flex', flexDirection: 'column', gap: space.xs, minWidth: 0, color: text.fg, fontSize: type.metaSize, lineHeight: type.metaLeading },
  totals: { display: 'flex', justifyContent: 'space-between', flexWrap: 'wrap', gap: space.md, fontVariantNumeric: 'tabular-nums' },
  session: { color: text.fg }, descendants: { color: text.fgSoft },
  chart: { display: 'block', width: '100%', height: 'auto', aspectRatio: '4 / 1' },
  note: { margin: 0, color: text.fgMuted, fontSize: type.denseSize },
  pendingText: { color: 'transparent', backgroundImage: `linear-gradient(${border.borderStrong}, ${border.borderStrong})`, backgroundSize: `100% ${space.md}`, backgroundPosition: 'center', backgroundRepeat: 'no-repeat', boxDecorationBreak: 'clone' },
  axis: { display: 'flex', justifyContent: 'space-between', color: text.fgMuted, fontSize: type.microSize, fontVariantNumeric: 'tabular-nums' },
})
