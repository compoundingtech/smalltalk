import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { userEvent, waitFor, within } from 'storybook/test'
import { DetailsPane, type DetailsPaneProps } from './assistant-ui/details/DetailsPane'
import { DetailsLabelContext } from './assistant-ui/details/DetailsResources'
import { detailsStoryInputs } from './assistant-ui/details/story-fixtures'
import { fullTraceFixture } from './assistant-ui/details/trace/fixture-provider'
import type { TraceFact, TraceSeries } from './assistant-ui/details/trace-series'
import type { DetailsOverview, DetailsSection } from './assistant-ui/details/model'
import { Workbench } from './assistant-ui/workbench/Workbench'
import { group } from './assistant-ui/workbench/workbench-model'
import { detailsWorkbenchResources as workbenchResources } from './assistant-ui/details/story-fixtures'
import { baselineTheme } from './assistant-ui/neutral-theme'
import { lightTheme } from './assistant-ui/composition-theme'
import { ThemePortal } from './assistant-ui/taste/ThemePortal'
import { surfaceVars as surface } from './assistant-ui/composition-tokens.stylex'

const now = 1800000000000
const known = <A,>(value: A): TraceFact<A> => ({ _tag: 'Known', value, source: 'st', asOfMs: now })
const unavailable = { _tag: 'Unknown', reason: 'not-attributed' } as const
const window = known({ _tag: 'Interval', fromMs: now - 3_600_000, toMs: now } as const)
const native: TraceSeries = {
  ...fullTraceFixture,
  coverage: { _tag: 'Complete' },
  meters: {
    ...fullTraceFixture.meters,
    spend: {
      ...fullTraceFixture.meters.spend,
      usdScope: { _tag: 'Self' }, usdWindow: window,
      totalUsdCoverage:{_tag:'Complete'},mainUsdScope:{_tag:'Self'},mainUsdWindow:window,mainUsdCoverage:{_tag:'Complete'},
      subagentUsdScope:{_tag:'IncludingSubagents'},subagentUsdWindow:window,subagentUsdCoverage:{_tag:'Unavailable',reason:'not-attributed'},
      tokensScope: { _tag: 'AgentIncarnations' }, tokensWindow: unavailable,
      totalUsd: known(12.34), mainUsd: known(12.34), subagentUsd: unavailable,
      tokens: known(480035214), split: unavailable,
    },
  },
  series: unavailable, turns: unavailable, subagents: unavailable, subagentsWindow: unavailable, subagentCount: unavailable,
}
const inclusive: TraceSeries = {
  ...native,
  meters: { ...native.meters, spend: { ...native.meters.spend,
    usdScope: { _tag: 'IncludingSubagents' }, totalUsd: known(15.55), subagentUsd: known(3.21),
    subagentUsdCoverage:{_tag:'Complete'},
    tokensScope: { _tag: 'Self' }, tokensWindow: window, tokens: known(120000),
  } },
  subagentCount: known(1),
  subagentsWindow:window,
  subagents: known([{ sessionId: 'native-child', parentSessionId: known('native-session'), label: known('Research delegate'), model: known('gpt-5.4'), usd: 3.21, requests: 4, firstMs: known(now - 1_800_000), lastMs: known(now - 60_000) }]),
}
const partial: TraceSeries = { ...inclusive, coverage: { _tag: 'Partial', attributedFraction: 0.62 }, subagentCount: known(4),
  meters:{...inclusive.meters,spend:{...inclusive.meters.spend,totalUsdCoverage:{_tag:'Partial',attributedFraction:0.62},subagentUsdCoverage:{_tag:'Partial',attributedFraction:0.62}}},
}
const withAttributionFraction=(fraction:number):TraceSeries=>({...partial,coverage:{_tag:'Partial',attributedFraction:fraction},
  meters:{...partial.meters,spend:{...partial.meters.spend,totalUsdCoverage:{_tag:'Partial',attributedFraction:fraction},subagentUsdCoverage:{_tag:'Partial',attributedFraction:fraction}}},
})
const boundary=withAttributionFraction(0.9949)
const boundary995=withAttributionFraction(0.995)
const zeroCoverage=withAttributionFraction(0)
const fullPartial=withAttributionFraction(1)
const notCovered: TraceSeries = { ...native, coverage: { _tag: 'NotCovered', harnessName: 'Claude Code' } }
const percentOnly: TraceSeries = { ...native, meters: { ...native.meters, context: {
  reportedPercent: known(37.4), usedTokens: unavailable, windowTokens: unavailable,
  compactions: unavailable, lastCompactionMs: unavailable,
} } }
const partialFacts: TraceSeries = { ...partial, subagents: known([
  { sessionId: 'native-child', parentSessionId: unavailable, label: unavailable, model: unavailable, usd: 3.21, requests: 4, firstMs: unavailable, lastMs: known(now - 60_000) },
]) }
const percentSamples: TraceSeries = { ...percentOnly, turns: known({
  points: [
    { turnIndex: 1, tMs: now - 120_000, contextTokens: null, contextPercent: 18.2, estimated: false },
    { turnIndex: 2, tMs: now - 60_000, contextTokens: null, contextPercent: 37.4, estimated: false },
  ], compactions: [],
}) }
const mixedWindows:TraceSeries={...partial,subagentsWindow:known({_tag:'Reported',label:'7d'}),meters:{...partial.meters,spend:{...partial.meters.spend,
 totalUsd:{_tag:'Known',value:100,source:'series'},usdWindow:known({_tag:'Reported',label:'7d'}),usdScope:{_tag:'IncludingSubagents'},
 mainUsd:known(12.34),mainUsdScope:{_tag:'Self'},mainUsdWindow:window,mainUsdCoverage:{_tag:'Complete'},
 subagentUsd:known(22),subagentUsdWindow:known({_tag:'Reported',label:'7d'}),
}}}
const unknownTotalScope:TraceSeries={...mixedWindows,meters:{...mixedWindows.meters,spend:{...mixedWindows.meters.spend,usdScope:{_tag:'Unknown',reason:'not-attributed'}}}}
const invalidCoverage=(fraction:number):TraceSeries=>({...inclusive,
  meters:{...inclusive.meters,spend:{...inclusive.meters.spend,totalUsdCoverage:{_tag:'Partial',attributedFraction:fraction}}},
})
const invalidNaN=invalidCoverage(Number.NaN),invalidNegative=invalidCoverage(-0.1),invalidOverflow=invalidCoverage(1.1)
const reportedLong: TraceSeries = { ...mixedWindows, meters: { ...mixedWindows.meters, spend: { ...mixedWindows.meters.spend,
  usdWindow: known({ _tag: 'Reported', label: 'last 7 days (as reported; exact cut bounds and attribution window are not supplied)' }),
} } }
const knownZero: TraceSeries = { ...native, subagents: known([]), subagentCount: known(0) }
const unknownCount: TraceSeries = { ...knownZero, subagentCount: unavailable }
const boundedCount: TraceSeries = { ...inclusive, subagentCount: known(309) }
const scenarios = { partial, boundary, boundary995, zeroCoverage, fullPartial, notCovered, independent: native, session: inclusive, partialFacts, percentOnly, percentSamples, mixedWindows, unknownTotalScope, invalidNaN, invalidNegative, invalidOverflow,
  reportedLong, knownZero, unknownCount, boundedCount, observedEmpty: native,
  firstRead: native, firstReadDense: native, firstReadCompact: native, firstReadHideEmpty: native, firstReadShowEmpty: native, settledUnavailable: native, knownEmpty: native, timing: native,
} as const
/** Observed st resources reported as an explicit empty list, independent of the traced breakdown. */
const sectionsForScenario = (scenario: string, sections: readonly DetailsSection[]): readonly DetailsSection[] =>
  ['knownZero', 'unknownCount', 'observedEmpty'].includes(scenario)
    ? sections.map(section => section.title === 'Subagents' ? { ...section, items: { _tag: 'Known' as const, value: [] } } : section)
    : sections
const unavailableSections: readonly DetailsSection[] = [
  { title: 'Workspace', items: { _tag: 'Unknown' }, unavailableReason: 'Workspace details are not supplied by this source.' },
  { title: 'Declared bindings', items: { _tag: 'Unknown' }, unavailableReason: 'Declared bindings are not supplied by this source.' },
  { title: 'Reviews', items: { _tag: 'Unknown' }, unavailableReason: 'Reviews are not supplied by this source.' },
]
const firstReadSections = unavailableSections.map(section => section.title === 'Reviews' ? section : { ...section, pending: true })
const emptySections: readonly DetailsSection[] = unavailableSections.map(section => ({ title: section.title, items: { _tag: 'Known', value: [] } }))
const resourceScenarios = {
  firstRead: { sections: firstReadSections, resources: 'R1', empty: 'E3' },
  firstReadDense: { sections: firstReadSections, resources: 'R2', empty: 'E3' },
  firstReadCompact: { sections: firstReadSections, resources: 'R3', empty: 'E3' },
  firstReadHideEmpty: { sections: firstReadSections, resources: 'R1', empty: 'E1' },
  firstReadShowEmpty: { sections: firstReadSections, resources: 'R1', empty: 'E2' },
  settledUnavailable: { sections: unavailableSections, resources: 'R1', empty: 'E3' },
  knownEmpty: { sections: emptySections, resources: 'R1', empty: 'E2' },
} as const
const elapsedLabel = 'Managed-session elapsed (from session start to end or now; wall time, not active)'
const ageLabel = 'Current-incarnation session age (from managed session start; native launch not reported)'
const spanLabel = 'Root observed span (first to last native observation)'
const timingGuard = 'The session start equals the producer snapshot time, so duration cannot be established.'
const timingProvenance = 'Observed by the host from native observations; not a managed session start.'
/** One first-read field beside a guarded missing value and a known value with host provenance. */
const timingDurations: DetailsOverview['durations'] = [
  { label: elapsedLabel, value: 'Loading', pending: true },
  { label: ageLabel, value: 'Unreported', detail: timingGuard },
  { label: spanLabel, value: '42m', detail: timingProvenance },
]
type ScenarioProps = Pick<DetailsPaneProps, 'sections' | 'variants' | 'overview'>
const scenarioProps = (scenario: string, props: ScenarioProps): ScenarioProps => {
  const resource = Object.entries(resourceScenarios).find(([name]) => name === scenario)?.[1]
  if (resource !== undefined) return { ...props, sections: resource.sections, variants: { ...props.variants, resources: resource.resources, empty: resource.empty } }
  if (scenario === 'timing') return { ...props, overview: { ...props.overview, durations: timingDurations } }
  return { ...props, sections: sectionsForScenario(scenario, props.sections) }
}
interface Args extends DetailsPaneProps { scenario: keyof typeof scenarios; scheme: 'dark' | 'light'; width: 240 | 320 | 400 | 520 }
const styles = stylex.create({
  frame: { height: 720, minHeight: 0, overflow: 'hidden', backgroundColor: surface.canvas },
  width: (width: number) => ({ width }),
  comparison: { display: 'flex', flexWrap: 'wrap', gap: 16, alignItems: 'flex-start' },
})
const meta = {
  title: 'Fractal UI/Details Data',
  component: DetailsPane,
  parameters: { layout: 'fullscreen' },
  args: {
    ...detailsStoryInputs('busy', native),
    variants: { overview: 'O2', context: 'C1', spend: 'S1', resources: 'R1', empty: 'E3' },
    state: 'busy', intent: { hiddenSections: [] }, onIntentChange: () => {}, onOpen: () => {}, onRetry: () => {},
    scenario: 'independent', scheme: 'dark', width: 400,
  },
  argTypes: {
    scenario: { control: 'select', options: Object.keys(scenarios) },
    scheme: { control: 'inline-radio', options: ['dark', 'light'] },
    width: { control: 'inline-radio', options: [240, 320, 400, 520] },
    trace: { control: false }, overview: { control: false }, sections: { control: false },
  },
  render: ({ scenario, scheme, width, ...props }) => <section aria-label={`Data case ${scenario}`} {...stylex.props(styles.frame, styles.width(width), ...baselineTheme, ...(scheme === 'light' ? lightTheme : []))}>
    <ThemePortal><Workbench resources={workbenchResources} layout={group([{ uri: 'agent:worker-1', view: 'details' }])} workspaceId={`details-data-${scenario}`} scheme={scheme}
      renderPane={() => <DetailsPane {...props} {...scenarioProps(scenario, props)} trace={scenarios[scenario]} pendingTrace={scenarios[scenario]} />}/></ThemePortal>
  </section>,
} satisfies Meta<Args>
export default meta
type Story = StoryObj<typeof meta>

function requireContract(condition: unknown, cause: string): asserts condition { if (!condition) throw new Error(cause) }
async function rejectsControl(cause: string, check: () => void | Promise<void>) {
  let rejected = false
  try { await check() } catch (error) { requireContract(error instanceof Error && error.message === cause, `Unexpected control failure: ${String(error)}`); rejected = true }
  requireContract(rejected, `Negative control did not reject: ${cause}`)
}
function pane(canvas: HTMLElement): HTMLElement {
  const element = canvas.querySelector<HTMLElement>('[data-testid="details-pane"]')
  requireContract(element, 'Missing production DetailsPane')
  requireContract(!/\bUnknown\b/.test(element.textContent ?? ''), 'Literal Unknown escaped the fact boundary')
  return element
}
function spend(pane: HTMLElement): HTMLElement {
  const element = pane.querySelector<HTMLElement>('[data-details-chart="S1"]')
  requireContract(element, 'Missing production spend section')
  return element
}
function assertPartial(root: HTMLElement, expected: string) {
  const labels = Array.from(root.querySelectorAll('[data-details-coverage]')).map(item => item.textContent)
  requireContract(labels.length > 0 && labels.every(label => label === expected), 'Coverage rounding or attribution copy differs')
  requireContract(!Array.from(spend(root).querySelectorAll('dt')).some(item => /^Total/.test(item.textContent ?? '')), 'Partial attribution was labelled a complete total')
}
function assertIndependent(root: HTMLElement, tokenLabel = 'Cumulative tokens · observed agent incarnations') {
  const section = spend(root)
  const tokenRow = section.querySelector('[data-details-token-label]')?.closest('div')
  requireContract(tokenRow?.querySelector('[data-details-token-label]')?.textContent === tokenLabel, 'Token scope inherited monetary scope')
  requireContract(!tokenRow?.querySelector('[data-details-token-period]')&&!tokenRow?.textContent?.includes('all time')&&tokenRow?.querySelector('button[aria-label="Cumulative tokens accumulation window: unavailable"]')?.textContent==='—', 'Token accumulation window was fabricated')
  const monetaryRow = Array.from(section.querySelectorAll('dt')).find(item => item.textContent?.startsWith('Total · self'))?.closest('div')
  const monetaryText = monetaryRow?.textContent
  requireContract(typeof monetaryText === 'string', 'Monetary accumulation window inherited token window')
  requireContract(monetaryText?.includes('UTC') && !monetaryText.includes('all time'), 'Monetary accumulation window inherited token window')
  requireContract(monetaryText.includes('$12.34') && tokenRow?.textContent?.includes('480'), 'Native metrics disappeared without descendants')
}
function assertPercentOnly(root: HTMLElement) {
  const section = root.querySelector('[data-details-chart^="C"]')
  requireContract(section, 'Missing context section')
  requireContract(!/\b\d[\d.,kKmM ]*\s+tokens\b|\bCapacity\b/.test(section.textContent ?? ''), 'Context occupancy rendered token counts')
  const meter = within(section as HTMLElement).getByRole('meter', { name: 'Context occupancy' })
  requireContract(meter.getAttribute('aria-valuemax') === '100' && meter.getAttribute('aria-valuenow') === '37.4', 'Reported-percent-only context was discarded')
}
function falsifyText(root:HTMLElement,selector:string,value:string):HTMLElement {
 const wrong=root.cloneNode(true) as HTMLElement
 const nodes=wrong.querySelectorAll(selector)
 requireContract(nodes.length>0,`Missing negative control target: ${selector}`)
 nodes.forEach(node=>{node.textContent=value})
 return wrong
}

/** Reported activity under partial observation: idle, working and omitted activity side by side. */
const activityOverviews = {
  idle: { ...detailsStoryInputs('idle').overview, activity: 'idle' },
  working: { ...detailsStoryInputs('busy').overview, activity: 'working' },
  unreported: { ...detailsStoryInputs('busy').overview, activity: undefined },
} satisfies Record<string, DetailsOverview>
export const PartialActivity: Story = {
  args: { state: 'partial' },
  render: ({ scheme, width, ...props }) => <div {...stylex.props(styles.comparison)}>{Object.entries(activityOverviews).map(([name, overview]) => <section key={name} aria-label={`${name} activity`} {...stylex.props(styles.frame, styles.width(width), ...baselineTheme, ...(scheme === 'light' ? lightTheme : []))}>
    <ThemePortal><Workbench resources={workbenchResources} layout={group([{ uri: 'agent:worker-1', view: 'details' }])} workspaceId={`details-data-activity-${name}`} scheme={scheme}
      renderPane={() => <DetailsLabelContext.Provider value={`${name} `}><DetailsPane {...props} {...scenarioProps('independent', props)} overview={overview} trace={native} pendingTrace={native} /></DetailsLabelContext.Provider>}/></ThemePortal>
  </section>)}</div>,
  play: async ({ canvasElement }) => {
    const overview = (name: keyof typeof activityOverviews) => within(pane(canvasElement.querySelector<HTMLElement>(`section[aria-label="${name} activity"]`)!)).getByRole('region', { name: `${name} Overview` })
    requireContract(within(overview('idle')).queryByRole('img', { name: /^Working;/ }) === null, 'Partial observation invented working activity')
    requireContract(within(overview('idle')).getByRole('img', { name: /^Idle;/ }), 'Reported idle activity disappeared')
    requireContract(within(overview('working')).getByRole('img', { name: /^Working;/ }), 'Partial observation hid reported working activity')
    requireContract(within(overview('unreported')).queryByRole('img') === null, 'Unreported activity invented a status glyph')
  },
}

/** Single-scenario contracts read their pane from AllStates, which mounts every scenario exactly as a lone story would. */
function scenarioPane(canvas: HTMLElement, name: keyof typeof scenarios): HTMLElement {
  const section = canvas.querySelector<HTMLElement>(`section[aria-label="${name}"]`)
  requireContract(section, 'AllStates omitted a named scenario')
  return pane(section)
}

/** Coverage copy rounds down, never claims 100% and keeps a legal 0%; each case falsifies its nearest wrong copy. */
const coverageCases = [
  { scenario: 'partial', expected: 'Partial · 62% attributed', wrong: ['Complete'] },
  { scenario: 'boundary', expected: 'Partial · 99% attributed', wrong: ['Partial · 100% attributed'] },
  { scenario: 'boundary995', expected: 'Partial · 99% attributed', wrong: ['Complete', 'Partial · 100% attributed'] },
  { scenario: 'zeroCoverage', expected: 'Partial · 0% attributed', wrong: ['Partial · 1% attributed'] },
  { scenario: 'fullPartial', expected: 'Partial · 99% attributed', wrong: ['Partial · 100% attributed'] },
] as const
async function checkCoverage(canvasElement: HTMLElement) {
  for (const { scenario, expected, wrong } of coverageCases) {
    const root = scenarioPane(canvasElement, scenario)
    assertPartial(root, expected)
    for (const value of wrong) await rejectsControl('Coverage rounding or attribution copy differs', () => assertPartial(falsifyText(root, '[data-details-coverage]', value), expected))
  }
  requireContract(scenarioPane(canvasElement, 'partial').textContent?.includes('Showing 1 of 4 traced sessions'), 'Retained descendants masqueraded as the full list')
}
async function checkNotCovered(canvasElement: HTMLElement) {
  const root = scenarioPane(canvasElement, 'notCovered')
  const trigger = within(spend(root)).getByRole('button', { name: 'Not tracked for Claude Code' })
  trigger.focus()
  requireContract(canvasElement.ownerDocument.activeElement === trigger, 'Coverage explanation is not keyboard reachable')
  await userEvent.keyboard('{Tab}{Shift>}{Tab}{/Shift}')
  await waitFor(() => requireContract(canvasElement.ownerDocument.activeElement === trigger, 'Keyboard navigation skipped the coverage trigger'))
  await waitFor(() => requireContract(within(canvasElement.ownerDocument.body).getByRole('tooltip').textContent === 'Spend is recorded only for OMP sessions today.', 'Not-covered explanation differs'))
  const wrong=falsifyText(root,'[data-details-coverage]','Not tracked for Codex')
  await rejectsControl('Not-covered harness display name differs', () => requireContract(wrong.querySelector('[data-details-coverage]')?.textContent === 'Not tracked for Claude Code', 'Not-covered harness display name differs'))
  requireContract(spend(root).textContent?.includes('$12.34') && spend(root).textContent?.includes('480'), 'Not-covered descendants erased native metrics')
  await userEvent.keyboard('{Escape}')
}
function assertDescendantListWindow(root:HTMLElement) {
 const label=root.querySelector('[data-details-subagent-window]')?.textContent
 requireContract(label==='Traced sessions · reported window · 7d','Descendant list inherited the native session window')
 const spendLabel=root.querySelector('[data-details-subagent-spend-window]')?.textContent
 requireContract(spendLabel===label,'Descendant spend breakdown lost its list window')
 requireContract(root.querySelector('[data-details-metric="main-usd"]')?.textContent?.includes('UTC'),'Native session lost its independent exact window')
}
async function checkDescendantListWindow(canvasElement: HTMLElement) {
  const root=scenarioPane(canvasElement,'mixedWindows')
  assertDescendantListWindow(root)
  const nativePeriod=root.querySelector('[data-details-metric="main-usd"] [data-details-metric-period]')?.textContent
  requireContract(nativePeriod,'Missing native-window negative control')
  const wrong=falsifyText(root,'[data-details-subagent-window]',nativePeriod)
  await rejectsControl('Descendant list inherited the native session window',()=>assertDescendantListWindow(wrong))
}
async function checkIndependentMetricScopes(canvasElement: HTMLElement) {
    const root = scenarioPane(canvasElement, 'independent')
    assertIndependent(root)
    await rejectsControl('Token scope inherited monetary scope', () => assertIndependent(falsifyText(root,'[data-details-token-label]','Cumulative tokens · this agent')))
    const monetaryWindow=spend(root).querySelector('[data-details-metric="total-usd"] [data-details-metric-period]')?.textContent
    requireContract(monetaryWindow,'Missing monetary-window negative control target')
    for(const inventedWindow of ['all time',monetaryWindow]) {
      const wrong=root.cloneNode(true) as HTMLElement
      const tokenLabel=wrong.querySelector('[data-details-token-label]')
      requireContract(tokenLabel?.parentElement,'Missing token-window negative control target')
      const period=wrong.ownerDocument.createElement('span')
      period.setAttribute('data-details-token-period','true')
      period.textContent=inventedWindow
      tokenLabel.parentElement.append(period)
      await rejectsControl('Token accumulation window was fabricated',()=>assertIndependent(wrong))
    }
}
function assertNativeWindow(root:HTMLElement) {
 const section=spend(root)
 const mainText=section.querySelector('[data-details-metric="main-usd"]')?.textContent
 requireContract(typeof mainText === 'string', 'Native monetary metadata inherited inclusive metadata')
 requireContract(mainText?.includes('Main session · self')&&!mainText.includes('reported window')&&mainText.includes('UTC')&&!mainText.includes('Partial'), 'Native monetary metadata inherited inclusive metadata')
 const totalText=section.querySelector('[data-details-metric="total-usd"]')?.textContent
 requireContract(typeof totalText === 'string', 'Producer window fabricated exact boundaries')
 requireContract(totalText?.includes('reported window · 7d')&&!totalText.includes('UTC'), 'Producer window fabricated exact boundaries')
 requireContract(mainText.includes('$12.34')&&totalText.includes('$100.00'), 'Independent monetary amounts were derived from one another')
}
async function checkNativeWindows(canvasElement: HTMLElement) {
  const root=scenarioPane(canvasElement,'mixedWindows')
  assertNativeWindow(root)
  const wrong=root.cloneNode(true) as HTMLElement
  const main=wrong.querySelector('[data-details-metric="main-usd"] dt')
  const total=wrong.querySelector('[data-details-metric="total-usd"] dt')
  requireContract(main&&total,'Missing monetary metadata control rows')
  main.textContent=total.textContent
  await rejectsControl('Native monetary metadata inherited inclusive metadata',()=>assertNativeWindow(wrong))
  const unattributed=scenarioPane(canvasElement,'unknownTotalScope')
  assertNativeWindow(unattributed)
  const withoutNative=unattributed.cloneNode(true) as HTMLElement
  withoutNative.querySelector('[data-details-metric="main-usd"]')?.remove()
  await rejectsControl('Native monetary metadata inherited inclusive metadata',()=>assertNativeWindow(withoutNative))
}
function assertSession(root:HTMLElement) {
 const section=spend(root)
 const total=section.querySelector('[data-details-metric="total-usd"]')
 const main=section.querySelector('[data-details-metric="main-usd"]')
 const descendants=section.querySelector('[data-details-metric="subagent-usd"]')
 requireContract(total?.textContent?.includes('Total · including subagents')&&total.querySelector('dd')?.textContent==='$15.55', 'Inclusive monetary total lost its scope')
 requireContract(main?.textContent?.includes('Main session')&&main.querySelector('dd')?.textContent==='$12.34'&&descendants?.textContent?.includes('Subagents')&&descendants.querySelector('dd')?.textContent==='$3.21', 'Session and descendants were conflated')
 requireContract(section.querySelector('[data-details-token-label]')?.textContent === 'Cumulative tokens · this agent', 'Self tokens inherited inclusive monetary scope')
}
function assertPartialFacts(root:HTMLElement) {
 const card=Array.from(root.querySelectorAll<HTMLElement>('[data-resource-card]')).find(item=>item.textContent?.includes('native-child'))
 requireContract(card, 'Unavailable label hid the known descendant identity')
 // Hover is supplementary: the card paints the reported facts, and they describe its open action.
 const facts=card.querySelector<HTMLElement>('[data-resource-detail]')
 requireContract(facts&&facts.checkVisibility({opacityProperty:true,visibilityProperty:true})&&facts.getBoundingClientRect().height>0&&card.querySelector('[data-details-resource-row]')?.getAttribute('aria-describedby')===facts.id, 'Reported descendant facts are hidden behind hover')
 const text=facts.textContent??''
 requireContract(!text.includes('Invalid Date')&&!text.includes('NaN')&&!text.includes('First observed')&&!text.includes('Parent ·')&&text.includes('Last observed'), 'Unavailable timestamps or parent fabricated lifecycle facts')
 requireContract(root.textContent?.includes('Showing 1 of 4 traced sessions'), 'Missing descendant facts changed the reported descendant count')
 requireContract(!card.textContent?.includes('gpt-5.4'), 'Unavailable model was fabricated')
 requireContract(card.querySelector('[data-details-resource-title]')?.textContent === 'role unreported · native-child', 'Missing descendant role was rendered as a bare identity')
}
function assertContextSamples(root:HTMLElement) {
 assertPercentOnly(root)
 const section=root.querySelector('[data-details-chart="C3"]')
 requireContract(section !== null, 'Percent-only retained observations were discarded')
 requireContract(section?.textContent?.includes('Peak 37%'), 'Percent-only retained observations were discarded')
 const titles=Array.from(section.querySelectorAll('circle title')).map(title=>title.textContent??'')
 requireContract(titles.length===2&&titles.every(title=>/\d+%/.test(title)&&!title.includes('tokens')), 'Historical occupancy escaped percentage units')
}
function assertResourceRead(root: HTMLElement, scenario: string, prefix: string) {
  const firstRead = scenario.startsWith('firstRead')
  requireContract(root.getAttribute('data-details-pane-state') === 'busy' && !root.querySelector('[data-details-skeleton="overview"]'), 'Resource read replaced loaded sibling details')
  for (const title of ['Workspace', 'Declared bindings']) {
    const section = root.querySelector<HTMLElement>(`section[aria-label="${prefix}${title}"]`)
    if (!firstRead && scenario !== 'knownEmpty') {
      const absent = root.querySelector(`[data-details-unreported-section="${title}"]`)
      requireContract(!section && absent, 'Settled absence lost its explicit unavailable reason')
      requireContract(!absent.hasAttribute('aria-busy'), 'Settled resource stayed pending')
      continue
    }
    requireContract(section, 'First-read or settled resource section was filtered out')
    const notes = [...section.querySelectorAll<HTMLElement>('[data-details-unavailable]')]
    if (firstRead) {
      requireContract(section.getAttribute('aria-busy') === 'true' && section.querySelector(`[data-details-skeleton="${title}"][aria-hidden="true"][inert] [data-details-skeleton-row]`), 'First-read resource rendered settled-unavailable instead of a pending skeleton')
      requireContract(notes.every(note => note.closest('[aria-hidden="true"][inert]')), 'First-read resource exposed settled-unavailable copy')
    } else {
      requireContract(!section.hasAttribute('aria-busy') && !section.querySelector('[data-details-skeleton]'), 'Settled resource stayed pending')
      if (scenario === 'knownEmpty') requireContract(notes.length === 0 && section.querySelector('p')?.textContent === 'None', 'Known-empty resources became unavailable')
    }
  }
  const reviews = root.querySelector<HTMLElement>(`section[aria-label="${prefix}Reviews"]`)
  if (scenario === 'knownEmpty') {
    requireContract(reviews && reviews.querySelector('p')?.textContent === 'None', 'Known-empty resources became unavailable')
  } else if (scenario === 'firstReadShowEmpty') {
    requireContract(reviews?.getAttribute('data-details-section-state') === 'unavailable' && !reviews.hasAttribute('aria-busy'), 'Static unreported section became pending')
  } else if (scenario !== 'firstReadHideEmpty') {
    const absent = root.querySelector('[data-details-unreported-section="Reviews"]')
    requireContract(!reviews && absent && !absent.hasAttribute('aria-busy'), 'Static unreported section became pending')
  }
}
function durationRow(root: HTMLElement, shortLabel: string): HTMLElement {
  const row = [...root.querySelectorAll<HTMLElement>('[data-details-description-row]')].find(item => item.querySelector('button')?.textContent === shortLabel)
  requireContract(row, 'Timing row is missing')
  return row
}
/** Timing fields settle independently; host provenance stays in each value's tooltip rather than repeating beneath values. */
function assertTiming(root: HTMLElement) {
  requireContract(root.getAttribute('data-details-pane-state') === 'busy' && !root.querySelector('[data-details-skeleton="overview"]') && root.querySelector('[data-details-name]')?.textContent === 'UI redesign milestone', 'Pending timing replaced loaded sibling details')
  const elapsed = durationRow(root, 'Managed-session elapsed'), age = durationRow(root, 'Current-incarnation session age'), span = durationRow(root, 'Root observed span')
  requireContract(elapsed.getAttribute('aria-busy') === 'true' && elapsed.querySelector('[data-details-skeleton="duration"][aria-hidden="true"][inert]') && elapsed.textContent?.endsWith('Loading') && !elapsed.querySelector('[data-details-missing-value]'), 'First-read timing rendered a settled value')
  requireContract(!age.hasAttribute('aria-busy') && !age.querySelector('[data-details-skeleton]') && age.querySelector(`[data-details-missing-value][aria-label="${ageLabel}: unavailable"]`)?.textContent === '—', 'Settled timing stayed pending')
  requireContract(!span.hasAttribute('aria-busy') && [...span.querySelectorAll('button')].some(button => button.textContent === '42m'), 'Known timing lost its value')
  requireContract(!root.textContent?.includes(timingGuard) && !root.textContent?.includes(timingProvenance), 'Timing guard reason repeats beneath values')
}
function assertAllStates(root:HTMLElement) {
 const names=Object.keys(scenarios)
 const mounted=root.querySelectorAll('[data-testid="details-pane"]')
 requireContract(mounted.length===names.length, 'AllStates omitted a production pane')
 for(const name of names)requireContract(root.querySelectorAll(`section[aria-label="${name}"] [data-testid="details-pane"]`).length===1, 'AllStates omitted a named scenario')
 mounted.forEach(element=>requireContract(!/\bUnknown\b/.test(element.textContent??''), 'Literal Unknown escaped the fact boundary'))
 const period = root.querySelector<HTMLElement>('section[aria-label="reportedLong"] [data-details-metric="total-usd"] [data-details-metric-period]')
 requireContract(period?.textContent === 'reported window · last 7 days (as reported; exact cut bounds and attribution window are not supplied)', 'Reported accumulation description was shortened')
 const knownZeroSection = root.querySelector<HTMLElement>('section[aria-label="knownZero"] section[aria-label="knownZero Subagents"]')
 requireContract(knownZeroSection?.querySelector('[data-details-subagent-count]')?.textContent === 'Showing 0 of 0 traced sessions', 'Known zero traced sessions disappeared under E3')
 requireContract(knownZeroSection?.querySelector('button[aria-expanded]')?.textContent === 'Subagents0', 'Known zero is absent from the Subagents heading')
 for (const name of ['knownZero', 'unknownCount', 'observedEmpty']) {
  const observed = root.querySelector(`section[aria-label="${name}"] [data-details-observed-subagent-count]`)
  requireContract(observed?.textContent === 'Observed st resources · 0', 'Known empty st resources became unreported')
 }
 for (const name of ['unknownCount', 'observedEmpty']) {
  const section = root.querySelector(`section[aria-label="${name}"] section[aria-label="${name} Subagents"]`)
  requireContract(section && !section.querySelector('[data-details-subagent-count]')?.textContent?.includes('of 0')
    && section.querySelector('button[aria-expanded]')?.textContent === 'Subagents', 'Unknown traced count became zero')
 }
 // Observed st rows precede the traced breakdown, so select the traced card by its session identity.
 const tracedTitles = [...root.querySelectorAll('section[aria-label="session"] section[aria-label="session Subagents"] [data-details-resource-title]')].filter(title => title.textContent?.includes('native-child'))
 requireContract(tracedTitles.length === 1 && tracedTitles[0]!.textContent === 'Research delegate · gpt-5.4 · native-child', 'Traced role, model or identity is hidden behind Open')
 const boundedSection = root.querySelector('section[aria-label="boundedCount"] section[aria-label="boundedCount Subagents"]')
 requireContract(boundedSection?.querySelector('button[aria-expanded]')?.textContent === 'Subagents309', 'Reported traced total disappeared from the Subagents heading')
 requireContract(boundedSection?.querySelector('[data-details-subagent-count]')?.textContent === 'Showing 1 of 309 traced sessions', 'Traced heading total became the retained list size')
 for (const name of Object.keys(resourceScenarios)) {
  const resourceRoot = root.querySelector<HTMLElement>(`section[aria-label="${name}"] [data-testid="details-pane"]`)
  requireContract(resourceRoot, 'AllStates omitted a resource-read control')
  assertResourceRead(resourceRoot, name, `${name} `)
 }
 const timingRoot = root.querySelector<HTMLElement>('section[aria-label="timing"] [data-testid="details-pane"]')
 requireContract(timingRoot, 'AllStates omitted the timing control')
 assertTiming(timingRoot)
}
async function checkSession(canvasElement: HTMLElement) {
    const root=scenarioPane(canvasElement,'session')
    assertSession(root)
    const wrong=falsifyText(root,'[data-details-metric="main-usd"] dd','$15.55')
    await rejectsControl('Session and descendants were conflated',()=>assertSession(wrong))
}
async function checkPartialDescendantFacts(canvasElement: HTMLElement) {
    const root = scenarioPane(canvasElement, 'partialFacts')
    assertPartialFacts(root)
    const card=Array.from(root.querySelectorAll<HTMLElement>('[data-resource-card]')).find(item=>item.textContent?.includes('native-child')),facts=card?.querySelector<HTMLElement>('[data-resource-detail]')
    requireContract(card&&facts,'Missing descendant facts negative control target')
    const original=facts.textContent,invented=card.ownerDocument.createElement('span')
    invented.textContent='gpt-5.4'
    try {
      card.append(invented)
      await rejectsControl('Unavailable model was fabricated',()=>assertPartialFacts(root))
      invented.remove()
      facts.textContent=(original??'').replace(/ · Last observed[^·]*/,'')
      await rejectsControl('Unavailable timestamps or parent fabricated lifecycle facts',()=>assertPartialFacts(root))
      facts.textContent=original
      facts.style.setProperty('visibility','hidden')
      await rejectsControl('Reported descendant facts are hidden behind hover',()=>assertPartialFacts(root))
      facts.style.removeProperty('visibility')
      const title=card.querySelector<HTMLElement>('[data-details-resource-title]')
      requireContract(title,'Missing role-wording negative control target')
      const titleText=title.textContent
      try {
        title.textContent='native-child'
        await rejectsControl('Missing descendant role was rendered as a bare identity',()=>assertPartialFacts(root))
      } finally { title.textContent=titleText }
    } finally { invented.remove(); facts.textContent=original; facts.style.removeProperty('visibility') }
    assertPartialFacts(root)
}
async function checkContextPercentOnly(canvasElement: HTMLElement) {
    const root = scenarioPane(canvasElement, 'percentOnly')
    assertPercentOnly(root)
    const wrong=root.cloneNode(true) as HTMLElement
    within(wrong).getByRole('meter',{name:'Context occupancy'}).setAttribute('aria-valuemax','1000000')
    await rejectsControl('Reported-percent-only context was discarded', () => assertPercentOnly(wrong))
}
export const ContextSamplesPercentOnly: Story = {
  args: { scenario: 'percentSamples', variants: { overview: 'O2', context: 'C3', spend: 'S1', resources: 'R1', empty: 'E3' } },
  play: async ({ canvasElement }) => {
    const root = pane(canvasElement)
    assertContextSamples(root)
    const wrong=falsifyText(root,'[data-details-chart="C3"] circle title','18 tokens · measured')
    await rejectsControl('Historical occupancy escaped percentage units',()=>assertContextSamples(wrong))
  },
}
function assertInvalidCoverageSurvives(canvas:HTMLElement,prefix:string) {
 const root=canvas.querySelector<HTMLElement>('[data-testid="details-pane"]')
 requireContract(root,'Invalid row blanked the Details pane')
 requireContract(root.querySelector('[data-details-metric="total-usd"] [data-details-coverage]')?.textContent==='Partial','Invalid fraction fabricated an attribution percentage')
 requireContract(root.querySelector('[data-details-metric="main-usd"]')?.textContent?.includes('$12.34')&&root.querySelector('[data-details-metric="subagent-usd"]')?.textContent?.includes('$3.21'),'Invalid coverage erased valid monetary rows')
 requireContract(root.querySelector(`section[aria-label="${prefix}Overview"]`)&&root.querySelector('[data-details-chart^="C"]')&&root.querySelector(`section[aria-label="${prefix}Workspace"]`),'Invalid coverage erased sibling sections')
}
async function checkInvalidCoverage(canvasElement: HTMLElement) {
 for(const name of ['invalidNaN','invalidNegative','invalidOverflow'] as const){
  const section=canvasElement.querySelector<HTMLElement>(`section[aria-label="${name}"]`)
  requireContract(section,'AllStates omitted a named scenario')
  assertInvalidCoverageSurvives(section,`${name} `)
  const blank=section.cloneNode(true) as HTMLElement
  blank.querySelector('[data-testid="details-pane"]')?.remove()
  await rejectsControl('Invalid row blanked the Details pane',()=>assertInvalidCoverageSurvives(blank,`${name} `))
  const percentage=falsifyText(section,'[data-details-metric="total-usd"] [data-details-coverage]','Partial · NaN% attributed')
  await rejectsControl('Invalid fraction fabricated an attribution percentage',()=>assertInvalidCoverageSurvives(percentage,`${name} `))
 }
}
export const AllStates: Story = {
  render: ({ scheme, width, ...props }) => <div {...stylex.props(styles.comparison)}>{Object.entries(scenarios).map(([name, trace]) => <section key={name} aria-label={name} {...stylex.props(styles.frame, styles.width(width), ...baselineTheme, ...(scheme === 'light' ? lightTheme : []))}>
    <ThemePortal><Workbench resources={workbenchResources} layout={group([{ uri: 'agent:worker-1', view: 'details' }])} workspaceId={`details-data-all-${name}`} scheme={scheme}
      renderPane={() => <DetailsLabelContext.Provider value={`${name} `}><DetailsPane {...props} {...scenarioProps(name, props)} trace={trace} pendingTrace={trace} /></DetailsLabelContext.Provider>}/></ThemePortal>
  </section>)}</div>,
  play:async({canvasElement})=>{
    assertAllStates(canvasElement)
    const wrong=canvasElement.cloneNode(true) as HTMLElement
    const missing=wrong.querySelector('[data-testid="details-pane"]')
    requireContract(missing,'Missing AllStates negative control target')
    missing.remove()
    await rejectsControl('AllStates omitted a production pane',()=>assertAllStates(wrong))
    await rejectsControl('Reported accumulation description was shortened',()=>assertAllStates(falsifyText(canvasElement,'section[aria-label="reportedLong"] [data-details-metric="total-usd"] [data-details-metric-period]','reported window · last 7 days')))
    await rejectsControl('Known zero is absent from the Subagents heading',()=>assertAllStates(falsifyText(canvasElement,'section[aria-label="knownZero Subagents"] button[aria-expanded]','Subagents')))
    await rejectsControl('Unknown traced count became zero',()=>assertAllStates(falsifyText(canvasElement,'section[aria-label="unknownCount Subagents"] button[aria-expanded]','Subagents0')))
    await rejectsControl('Traced heading total became the retained list size',()=>assertAllStates(falsifyText(canvasElement,'section[aria-label="boundedCount Subagents"] [data-details-subagent-count]','Showing 1 of 1 traced sessions')))
    await rejectsControl('Traced role, model or identity is hidden behind Open',()=>assertAllStates(falsifyText(canvasElement,'section[aria-label="session Subagents"] [data-resource-card] [data-details-resource-title]','Research delegate · native-child')))
    await rejectsControl('First-read resource rendered settled-unavailable instead of a pending skeleton',()=>assertAllStates(mutated(canvasElement,'section[aria-label="firstReadDense Workspace"] [data-details-skeleton]',node=>node.removeAttribute('data-details-skeleton'))))
    await rejectsControl('Settled absence lost its explicit unavailable reason',()=>assertAllStates(mutated(canvasElement,'section[aria-label="settledUnavailable"] [data-details-unreported-section="Workspace"]',node=>node.remove())))
    await rejectsControl('Settled resource stayed pending',()=>assertAllStates(mutated(canvasElement,'section[aria-label="settledUnavailable"] [data-details-unreported-section="Workspace"]',node=>node.setAttribute('aria-busy','true'))))
    await rejectsControl('Known-empty resources became unavailable',()=>assertAllStates(falsifyText(canvasElement,'section[aria-label="knownEmpty Workspace"] p','Unavailable')))
    await rejectsControl('First-read timing rendered a settled value',()=>assertAllStates(mutated(canvasElement,'section[aria-label="timing"] [data-details-skeleton="duration"]',node=>node.remove())))
    // Every scenario pane here is the state a single-scenario story would mount, so its contract runs in place.
    for (const check of [checkCoverage, checkNotCovered, checkDescendantListWindow, checkIndependentMetricScopes, checkNativeWindows, checkSession, checkPartialDescendantFacts, checkContextPercentOnly, checkInvalidCoverage, checkTiming, checkFirstReadResources]) await check(canvasElement)
  },
}
/** Clones the canvas and applies one mutation to every match, so the live tree stays intact. */
function mutated(root: HTMLElement, selector: string, mutate: (node: HTMLElement) => void): HTMLElement {
  const wrong = root.cloneNode(true) as HTMLElement
  const nodes = wrong.querySelectorAll<HTMLElement>(selector)
  requireContract(nodes.length > 0, `Missing negative control target: ${selector}`)
  nodes.forEach(mutate)
  return wrong
}
function unclipped(element: Element | null): boolean {
  return element instanceof HTMLElement && element.scrollWidth <= element.clientWidth + 1 && element.getBoundingClientRect().height > 0
}
/** A reported window keeps its full description at the narrowest pane, wrapping between words instead of clipping. */
function assertReportedNarrow(root: HTMLElement) {
  const row = root.querySelector<HTMLElement>('[data-details-metric="total-usd"]')
  const period = row?.querySelector<HTMLElement>('[data-details-metric-period]')
  requireContract(row && period?.textContent === 'reported window · last 7 days (as reported; exact cut bounds and attribution window are not supplied)', 'Reported accumulation description was shortened')
  requireContract(unclipped(period) && unclipped(row.querySelector('dt')) && unclipped(row), 'Reported accumulation description clips instead of wrapping')
  const amount = row.querySelector('dd')
  requireContract(amount?.textContent === '$100.00' && unclipped(amount) && amount.getBoundingClientRect().right <= row.getBoundingClientRect().right + 1, 'Reported spend amount is clipped beside its description')
}
export const ReportedWindowNarrow: Story = {
  args: { scenario: 'reportedLong', width: 240 },
  play: async ({ canvasElement }) => {
    const root = pane(canvasElement)
    await waitFor(() => assertReportedNarrow(root))
    const parts = [...root.querySelectorAll<HTMLElement>('[data-details-metric="total-usd"] [data-details-metric-period] span span')]
    requireContract(parts.length > 0, 'Missing reported-window negative control target')
    try {
      parts.forEach(part => part.style.setProperty('white-space', 'nowrap'))
      await rejectsControl('Reported accumulation description clips instead of wrapping', () => assertReportedNarrow(root))
    } finally { parts.forEach(part => part.style.removeProperty('white-space')) }
    await waitFor(() => assertReportedNarrow(root))
  },
}
/** Opens a value's tooltip from the keyboard and returns it once it carries the expected text, skipping a closing sibling. */
async function tooltipFor(canvas: HTMLElement, trigger: HTMLElement, expected: string, cause: string): Promise<HTMLElement> {
  trigger.focus()
  requireContract(canvas.ownerDocument.activeElement === trigger, 'Timing value is not keyboard reachable')
  await userEvent.keyboard('{Tab}{Shift>}{Tab}{/Shift}')
  await waitFor(() => requireContract(canvas.ownerDocument.activeElement === trigger, 'Keyboard navigation skipped the timing value'))
  let tooltip: HTMLElement | undefined
  await waitFor(() => {
    tooltip = within(canvas.ownerDocument.body).queryAllByRole('tooltip').find(item => item.textContent === expected)
    requireContract(tooltip, cause)
  })
  requireContract(tooltip, cause)
  return tooltip
}
async function checkTiming(canvasElement: HTMLElement) {
    const root = scenarioPane(canvasElement, 'timing')
    await waitFor(() => assertTiming(root))
    await rejectsControl('First-read timing rendered a settled value', () => assertTiming(mutated(root, '[data-details-skeleton="duration"]', node => node.remove())))
    await rejectsControl('Settled timing stayed pending', () => assertTiming(mutated(root, `[data-details-missing-value][aria-label="${ageLabel}: unavailable"]`, node => node.closest('[data-details-description-row]')?.setAttribute('aria-busy', 'true'))))
    await rejectsControl('Timing guard reason repeats beneath values', () => assertTiming(mutated(root, `[data-details-missing-value][aria-label="${ageLabel}: unavailable"]`, node => { const prose = node.ownerDocument.createElement('p'); prose.textContent = timingGuard; node.closest('[data-details-description-row]')?.after(prose) })))
    const missing = root.querySelector<HTMLElement>(`[data-details-missing-value][aria-label="${ageLabel}: unavailable"]`)
    requireContract(missing, 'Missing guarded timing value')
    const guard = await tooltipFor(canvasElement, missing, timingGuard, 'Guarded timing tooltip lost the host reason')
    const wrongGuard = guard.cloneNode(true) as HTMLElement
    wrongGuard.textContent = `${ageLabel} is not reported by this source.`
    await rejectsControl('Guarded timing tooltip lost the host reason', () => requireContract(wrongGuard.textContent === timingGuard, 'Guarded timing tooltip lost the host reason'))
    const value = [...durationRow(root, 'Root observed span').querySelectorAll<HTMLElement>('button')].find(button => button.textContent === '42m')
    requireContract(value, 'Missing known timing value')
    const provenance = await tooltipFor(canvasElement, value, `42m${timingProvenance}`, 'Known timing tooltip lost host provenance')
    requireContract(provenance.querySelector('[data-details-value-detail]')?.textContent === timingProvenance, 'Known timing tooltip lost host provenance')
    await userEvent.keyboard('{Escape}')
}
async function checkFirstReadResources(canvasElement: HTMLElement) {
    const root = scenarioPane(canvasElement, 'firstRead')
    const prefix = 'firstRead '
    await waitFor(() => assertResourceRead(root, 'firstRead', prefix))
    await rejectsControl('First-read resource rendered settled-unavailable instead of a pending skeleton', () => assertResourceRead(mutated(root, `section[aria-label="${prefix}Workspace"] [data-details-skeleton]`, node => node.removeAttribute('data-details-skeleton')), 'firstRead', prefix))
    await rejectsControl('First-read resource exposed settled-unavailable copy', () => assertResourceRead(mutated(root, `section[aria-label="${prefix}Workspace"]`, node => { const note = document.createElement('p'); note.setAttribute('data-details-unavailable', ''); node.append(note) }), 'firstRead', prefix))
    await rejectsControl('Static unreported section became pending', () => assertResourceRead(mutated(root, '[data-details-unreported-section="Reviews"]', node => node.setAttribute('aria-busy', 'true')), 'firstRead', prefix))
    await rejectsControl('Resource read replaced loaded sibling details', () => assertResourceRead(mutated(root, '[data-details-content-slot]', node => node.closest('[data-testid="details-pane"]')?.setAttribute('data-details-pane-state', 'loading')), 'firstRead', prefix))
    await rejectsControl('First-read or settled resource section was filtered out', () => assertResourceRead(mutated(root, `section[aria-label="${prefix}Declared bindings"]`, node => node.remove()), 'firstRead', prefix))
    const trigger = root.querySelector<HTMLElement>('[data-details-unreported-section="Reviews"]')
    requireContract(trigger, 'Reviews absence is not keyboard reachable')
    trigger.focus()
    await userEvent.keyboard('{Tab}{Shift>}{Tab}{/Shift}')
    await waitFor(() => requireContract(canvasElement.ownerDocument.activeElement === trigger, 'Keyboard navigation skipped the Reviews absence'))
    await waitFor(() => requireContract(within(canvasElement.ownerDocument.body).getByRole('tooltip').textContent === 'Reviews are not supplied by this source.', 'Reviews absence lost its source reason'))
    await userEvent.keyboard('{Escape}')
}
