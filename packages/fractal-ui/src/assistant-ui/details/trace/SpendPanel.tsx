import * as stylex from '@stylexjs/stylex'
import type { ReactNode } from 'react'
import { accentVars as accent, borderVars as border, spaceVars as space, surfaceVars as surface, textVars as text, typeVars as type } from '../../composition-tokens.stylex.ts'
import { areaPath, baseline, CHART_HEIGHT, CHART_INSET, CHART_WIDTH, clock, linePath, money, xFor, yFor } from './chart.ts'
import type { ChartScene as AgentTraceScene, ChartLane as Lane } from './adapter.ts'
import { DetailsInk, DetailsSkeletonPlot } from '../DetailsSkeleton.tsx'
import type { TraceCostSplit } from '../trace-series.ts'
import { MissingDetailsValue } from '../DetailsUnavailable.tsx'
type SpendComponents = { readonly input: number; readonly cacheRead: number; readonly cacheWrite: number; readonly output: number }
const SPEND_COMPONENTS: ReadonlyArray<{ readonly id: keyof SpendComponents; readonly label: string; readonly color: string }> = [
  { id: 'input', label: 'Input', color: accent.primary },
  { id: 'cacheRead', label: 'Cache read', color: text.fgMuted },
  { id: 'cacheWrite', label: 'Cache write', color: text.fgSoft },
  { id: 'output', label: 'Output', color: text.fg },
]
const hasComponents = (components: SpendComponents): boolean => Object.values(components).every(Number.isFinite)

export interface SpendPanelProps {
  readonly scene?: AgentTraceScene
  readonly lane?: Lane
  readonly componentsKnown?: boolean
  readonly showTotals?: boolean
  /** Reported components remain usable even when their historical marks are missing. */
  readonly split?: TraceCostSplit
}

/** SpendPanel's retained-evidence aggregation and stacked SVG, plus the four-way split bars. */
export const SpendPanel = ({ lane, componentsKnown = true, showTotals = true, split }: SpendPanelProps): ReactNode => {
  const componentSamples = lane?.samples.map((sample) => ({ ts: sample.ts, components: sample.cumulativeUsdByComponent })) ?? []
  const attributed = componentsKnown && componentSamples.length > 0 && componentSamples.every(({ components }) => hasComponents(components))
  const cumulativeValues = lane?.cumulativeValues ?? []
  const hasSeries = cumulativeValues.length > 0
  const start = componentSamples[0]?.ts ?? cumulativeValues[0]?.ts ?? 0
  const end = componentSamples.at(-1)?.ts ?? cumulativeValues.at(-1)?.ts ?? start
  const retainedUsd = cumulativeValues.at(-1)?.usd ?? 0
  const partial = lane?.totalUsd._tag === 'Known' && Math.abs(retainedUsd - lane.totalUsd.value) > 0.005
  const maximum = Math.max(0.000001, retainedUsd)
  const totalPath = linePath(cumulativeValues.map((point) => ({ x: xFor(point.ts, start, end), y: yFor(point.usd, maximum) })))
  const reportedComponents = split === undefined ? undefined : { input: split.uncachedInputUsd, cacheRead: split.cacheReadUsd, cacheWrite: split.cacheWriteUsd, output: split.outputUsd }
  const lastComponents = attributed ? componentSamples.at(-1)?.components : reportedComponents
  const componentTotals = lastComponents !== undefined && hasComponents(lastComponents) ? SPEND_COMPONENTS.map((component) => ({ ...component, value: lastComponents[component.id] })) : []
  const attributedTotal = componentTotals.reduce((total, component) => total + component.value, 0)
  return <div {...stylex.props(styles.root)}>
    <div data-details-data-row={showTotals && lane?.totalUsd._tag === 'Known' ? true : undefined} {...stylex.props(styles.header)}>{showTotals && <span>Main session <strong>{lane?.totalUsd._tag === 'Known' ? money.format(lane.totalUsd.value) : <MissingDetailsValue label="Main session spend" reason={lane?.totalUsd._tag === 'Unknown' ? lane.totalUsd.detail ?? lane.totalUsd.reason : 'Session spend total was not supplied.'}/>}</strong></span>}<span {...stylex.props(styles.note)}>API-equivalent</span></div>
    {hasSeries ? <>
      <svg {...stylex.props(styles.chart)} viewBox={`0 0 ${CHART_WIDTH} ${CHART_HEIGHT}`} preserveAspectRatio="none" role="img" aria-label={attributed ? 'Cumulative API-equivalent spend split by input, cache read, cache write and output' : 'Cumulative main-session API-equivalent spend'}>
        <path d={`M${CHART_INSET} ${baseline}H${CHART_WIDTH - CHART_INSET}`} stroke={border.borderStrong} />
        {attributed && SPEND_COMPONENTS.map((component, componentIndex) => {
          const lower = componentSamples.map(({ ts, components }) => ({ x: xFor(ts, start, end), y: yFor(SPEND_COMPONENTS.slice(0, componentIndex).reduce((total, candidate) => total + components[candidate.id], 0), maximum) }))
          const upper = componentSamples.map(({ ts, components }) => ({ x: xFor(ts, start, end), y: yFor(SPEND_COMPONENTS.slice(0, componentIndex + 1).reduce((total, candidate) => total + components[candidate.id], 0), maximum) }))
          return <path key={component.id} d={areaPath(upper, lower)} fill={component.color} fillOpacity={0.24}><title>{component.label}</title></path>
        })}
        <path d={totalPath} fill="none" stroke={text.fg} strokeWidth={1.5} vectorEffect="non-scaling-stroke" />
      </svg>
      <div {...stylex.props(styles.axis)}><span>{clock.format(start)}</span><span>UTC</span><span>{clock.format(end)}</span></div>
    </> : <span {...stylex.props(styles.note)}>Spend series <MissingDetailsValue label="Spend series" reason="Session spend series was not retained."/></span>}
    {componentTotals.length === 0 && <span {...stylex.props(styles.note)}>Spend split <MissingDetailsValue label="Four-way spend split" reason="Provider does not supply component attribution."/></span>}
    {componentTotals.length > 0 && <dl {...stylex.props(styles.breakdown)} aria-label="Four-way session spend split">{componentTotals.map((component) => <div key={component.id} {...stylex.props(styles.component)}>
      <dt {...stylex.props(styles.label)}>{component.label}</dt>
      <dd {...stylex.props(styles.bar)}><svg {...stylex.props(styles.splitBar)} viewBox="0 0 100 8" preserveAspectRatio="none" aria-hidden="true"><rect width={100} height={8} fill={surface.rowActive} /><rect width={attributedTotal > 0 ? component.value / attributedTotal * 100 : 0} height={8} fill={component.color} /></svg></dd>
      <dd {...stylex.props(styles.value)}>{money.format(component.value)}</dd>
    </div>)}</dl>}
    {partial && hasSeries && <p {...stylex.props(styles.note)}>Chart and split show retained observations; reported accumulation windows may differ.</p>}
  </div>
}

/** Loading twin of SpendPanel: same wrappers, gaps and chart box, ink instead of values. */
export const SpendPanelPending = ({showTotals=true,footprint={series:true,split:true,partial:false}}:{readonly showTotals?:boolean;readonly footprint?:{readonly series:boolean;readonly split:boolean;readonly partial:boolean}}): ReactNode => <div aria-hidden="true" inert {...stylex.props(styles.root)}>
  <div {...stylex.props(styles.header)}>{showTotals&&<span><span {...stylex.props(styles.pendingText)}>Main session <strong>$0.00</strong></span></span>}<span {...stylex.props(styles.note)}><span {...stylex.props(styles.pendingText)}>API-equivalent</span></span></div>
  {footprint.series ? <><DetailsSkeletonPlot /><div {...stylex.props(styles.axis)}><span><DetailsInk ch={5} /></span><span><span {...stylex.props(styles.pendingText)}>UTC</span></span><span><DetailsInk ch={5} /></span></div></> : <span {...stylex.props(styles.note)}><span {...stylex.props(styles.pendingText)}>Spend series —</span></span>}
  {!footprint.split && <span {...stylex.props(styles.note)}><span {...stylex.props(styles.pendingText)}>Spend split —</span></span>}
  {footprint.split && <dl {...stylex.props(styles.breakdown)}>{SPEND_COMPONENTS.map((component) => <div key={component.id} {...stylex.props(styles.component)}>
    <dt {...stylex.props(styles.label)}><span {...stylex.props(styles.pendingText)}>{component.label}</span></dt>
    <dd {...stylex.props(styles.bar)}><span {...stylex.props(styles.pendingBar)} /></dd>
    <dd {...stylex.props(styles.value)}><DetailsInk ch={5} /></dd>
  </div>)}</dl>}
  {footprint.partial && footprint.series && <p {...stylex.props(styles.note)}><span {...stylex.props(styles.pendingText)}>Chart and split show retained observations; reported accumulation windows may differ.</span></p>}
</div>

const styles = stylex.create({
  root: { display: 'flex', flexDirection: 'column', gap: space.xs, minWidth: 0, color: text.fg, fontSize: type.metaSize, lineHeight: type.metaLeading },
  header: { display: 'flex', justifyContent: 'space-between', gap: space.md, flexWrap: 'wrap', fontVariantNumeric: 'tabular-nums' },
  chart: { display: 'block', width: '100%', height: 'auto', aspectRatio: '320 / 112' },
  note: { margin: 0, color: text.fgMuted, fontSize: type.denseSize },
  pendingText: { color: 'transparent', backgroundImage: `linear-gradient(${border.borderStrong}, ${border.borderStrong})`, backgroundSize: `100% ${space.md}`, backgroundPosition: 'center', backgroundRepeat: 'no-repeat', boxDecorationBreak: 'clone' },
  axis: { display: 'flex', justifyContent: 'space-between', color: text.fgMuted, fontSize: type.microSize, fontVariantNumeric: 'tabular-nums' },
  breakdown: { display: 'flex', flexDirection: 'column', gap: space.xs, margin: 0 },
  component: { display: 'grid', gridTemplateColumns: 'max-content minmax(0, 1fr) max-content', gap: space.md, alignItems: 'center', fontVariantNumeric: 'tabular-nums' },
  label: { minWidth: '10ch', color: text.fgMuted },
  bar: { margin: 0, minWidth: 0 },
  splitBar: { display: 'block', height: space.md, width: '100%' },
  pendingBar: { display: 'block', height: space.md, width: '100%', backgroundColor: surface.rowActive },
  value: { margin: 0, fontVariantNumeric: 'tabular-nums' },
})
