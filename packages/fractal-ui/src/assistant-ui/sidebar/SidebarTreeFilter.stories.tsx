import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { expect, userEvent, waitFor, within } from 'storybook/test'
import type { Key } from 'react-aria-components'
import { SidebarTree, type SidebarTreeItem } from './SidebarTree'
import { SidebarAgentRow, SidebarQueryContext } from './SidebarAgentRow'
import { matchPositions, type SidebarAgentRow as Row } from './model'
import { lightTheme } from '../composition-theme'
import { borderVars as border, geometryVars as g, spaceVars as s, surfaceVars as surface, textVars as ink, typeVars as t } from '../composition-tokens.stylex'
import { sidebarTreeStoryVars as sizes } from './sidebar-tree-story-tokens.stylex'

interface Agent extends SidebarTreeItem<Agent> {}
// Original synthetic agents, not application data. Filter only the array: every
// retained object is the production fixture object RAC previously received.
const fixture: readonly Agent[] = Array.from({ length: 78 }, (_, index) => ({
  id: `filter-agent-${index + 1}`,
  depth: 0,
  textValue: `${['Brindle scout', 'Aster flux', 'Cobalt surveyor'][index % 3]} ${String(Math.floor(index / 3) + 1).padStart(2, '0')}`,
}))
const emptyExpanded = new Set<Key>()
const onExpandedChange = (_keys: Set<Key>) => {}
const onRowContextMenu = (_item: Agent, event: React.MouseEvent) => event.preventDefault()
/** Samples per input per phase; p90 is the 27th of 30, so three load spikes cannot set it alone. */
const sampleCount = 30
/** Asynchronous settling on a contended CI host; the samples themselves carry the measurements. */
const settled = { timeout: 10_000 }
/** Filter keystroke-to-next-frame p90 may be at most this multiple of the reference keystroke p90. */
const ratioBudget = 3
/** The synchronous regression control: each filter keystroke busy-waits this multiple of its paired reference keystroke. */
const controlRatio = 4
/** Renders are attributed to a keystroke through this many frames after its first post-commit frame. */
const settleFrames = 2

interface RenderCall { readonly key: string; readonly pinned: boolean; readonly retained: boolean }
interface Sample {
  readonly durationMs: number
  readonly value: string
  readonly count: number
  readonly retainedIdentity: boolean
  readonly paintedKeys: readonly string[]
  /** Painted rows whose keyed DOM row is new for this keystroke: newly mounted, or a reused RAC view rebound to the key. */
  readonly changedKeys: readonly string[]
  readonly renders?: readonly RenderCall[]
}
interface PendingInput { readonly start: number; readonly value: string; readonly rowsBefore: ReadonlyMap<string, Element> }

const paintedRows = (element: HTMLElement) => new Map([...element.querySelectorAll<HTMLElement>('[data-sidebar-tree-key]')].map(row => [row.dataset.sidebarTreeKey!, row] as const))
const nextFrame = () => new Promise<void>(resolve => requestAnimationFrame(() => resolve()))

/**
 * Times input capture through the first animation frame after React commits (the
 * browser's next-paint boundary, not a physical display-paint claim), then attaches
 * every renderRow call made until `settleFrames` later, so deferred work is counted.
 */
function useKeystrokeSample(value: string, pending: React.RefObject<PendingInput | undefined>, record: (input: PendingInput, durationMs: number) => Sample, settle: (sample: Sample) => void) {
  const recordEvent = React.useEffectEvent(record)
  const settleEvent = React.useEffectEvent(settle)
  React.useLayoutEffect(() => {
    const input = pending.current
    if (input === undefined || input.value !== value) return
    let cancelled = false
    const frame = requestAnimationFrame(async () => {
      // Use now(), not the frame timestamp: the latter can precede a costly input handler.
      const durationMs = performance.now() - input.start
      pending.current = undefined
      const sample = recordEvent(input, durationMs)
      for (let index = 0; index < settleFrames; index++) await nextFrame()
      if (!cancelled) settleEvent(sample)
    })
    return () => { cancelled = true; cancelAnimationFrame(frame) }
  }, [value, pending])
}

function FilterProbe({ light, narrow }: { readonly light: boolean; readonly narrow: boolean }) {
  const id = `${light ? 'light' : 'dark'}-${narrow ? '240' : '440'}`
  const [query, setQuery] = React.useState('')
  const [reference, setReference] = React.useState('')
  const root = React.useRef<HTMLElement>(null)
  const pendingFilter = React.useRef<PendingInput | undefined>(undefined)
  const pendingReference = React.useRef<PendingInput | undefined>(undefined)
  const renders = React.useRef<RenderCall[]>([])
  const filterSamples = React.useRef<Sample[]>([])
  const referenceSamples = React.useRef<Sample[]>([])
  const items = React.useMemo(() => {
    const filtered = query === '' ? fixture : fixture.filter(item => item.textValue.toLowerCase().includes(query.toLowerCase()))
    // Rebuild control: fresh item objects on every keystroke, as a host that rebuilds
    // its projection would hand RAC. The work and identity checks must reject it.
    return root.current?.dataset.rebuildControl === 'true' ? filtered.map(item => ({ ...item })) : filtered
  }, [query])
  const renderRow = React.useCallback((item: Agent, state: { readonly pinned: boolean }) => {
    renders.current.push({ key: String(item.id), pinned: state.pinned, retained: fixture.includes(item) })
    return <div {...stylex.props(styles.row)}>{item.textValue}</div>
  }, [])
  const capture = (pending: React.RefObject<PendingInput | undefined>) => (event: React.FormEvent<HTMLInputElement>) => {
    // Start at input capture, BEFORE onChange's synchronous work and React's commit.
    renders.current = []
    pending.current = { start: performance.now(), value: event.currentTarget.value, rowsBefore: root.current === null ? new Map() : paintedRows(root.current) }
  }
  const publish = () => {
    const element = root.current!
    element.dataset.filterSamples = JSON.stringify(filterSamples.current)
    element.dataset.referenceSamples = JSON.stringify(referenceSamples.current)
  }
  const recordInto = (samples: React.RefObject<Sample[]>, count: number) => (input: PendingInput, durationMs: number) => {
    const rows = paintedRows(root.current!)
    const changedKeys = [...rows].filter(([key, row]) => input.rowsBefore.get(key) !== row).map(([key]) => key)
    const sample: Sample = { durationMs, value: input.value, count, retainedIdentity: items.every(item => fixture.includes(item)), paintedKeys: [...rows.keys()], changedKeys }
    samples.current.push(sample)
    return sample
  }
  const settleInto = (samples: React.RefObject<Sample[]>) => (sample: Sample) => {
    const index = samples.current.indexOf(sample)
    if (index < 0) return
    samples.current[index] = { ...sample, renders: [...renders.current] }
    publish()
  }
  useKeystrokeSample(query, pendingFilter, recordInto(filterSamples, items.length), settleInto(filterSamples))
  useKeystrokeSample(reference, pendingReference, recordInto(referenceSamples, items.length), settleInto(referenceSamples))
  const changeQuery = (event: React.ChangeEvent<HTMLInputElement>) => {
    const cost = Number(root.current?.dataset.synchronousCostMs ?? 0)
    if (cost > 0) {
      const until = performance.now() + cost
      while (performance.now() < until) { /* Deliberate synchronous regression control. */ }
    }
    setQuery(event.currentTarget.value)
  }
  return <section ref={root} data-testid={`filter-probe-${id}`} data-query={query} data-synchronous-cost-ms="0" data-rebuild-control="false" data-visible-count={items.length} {...stylex.props(styles.probe, light && lightTheme, narrow ? styles.narrow : styles.wide)}>
    <div {...stylex.props(styles.toolbar)}>
      <span>{light ? 'Light' : 'Dark'} · {narrow ? '240' : '440'} · 78 agents</span>
      <input aria-label={`Filter agents ${id}`} type="search" value={query} onInputCapture={capture(pendingFilter)} onChange={changeQuery} {...stylex.props(styles.input)} />
      <input aria-label={`Reference keystroke ${id}`} type="text" value={reference} onInputCapture={capture(pendingReference)} onChange={event => setReference(event.currentTarget.value)} {...stylex.props(styles.input)} />
    </div>
    <div {...stylex.props(styles.viewport)}>
      <SidebarTree aria-label={`Filtered agents ${id}`} items={items} expandedKeys={emptyExpanded} onExpandedChange={onExpandedChange} renderRow={renderRow} onRowContextMenu={onRowContextMenu} />
    </div>
  </section>
}

interface AgentEntry extends SidebarTreeItem<AgentEntry> { readonly row: Row }
const highlightNow = Date.UTC(2026, 9, 8, 12, 0, 0)
/** Matches no fixture title: the control's context value, which the explicit row prop must override. */
const decoyQuery = 'qqq'
/** Every SidebarAgentRow render reads its item's title once (`sidebarRowTitle`); the getter counts those reads per ref. */
const titleReads = new Map<string, number>()
// Original synthetic agents reporting only the always-present facts. Names follow the
// host's formula: the visible title first, then the status.
const agentEntry = (title: string, slug: string): AgentEntry => {
  const ref = `agent/${slug}`
  const facts: Omit<Row, 'title'> = { ref, id: slug, status: 'idle', statusLabel: 'Idle', host: 'host-1', freshness: 'live', usage: { _tag: 'Unknown' }, duration: { _tag: 'Unknown' }, lastTurn: { _tag: 'Unknown' }, children: [] }
  const row = Object.defineProperty(facts, 'title', { enumerable: true, get: () => { titleReads.set(ref, (titleReads.get(ref) ?? 0) + 1); return title } }) as Row
  return { id: ref, depth: 0, textValue: title, accessibleName: `${title}, ${facts.statusLabel}`, row }
}
const highlightFixture: readonly AgentEntry[] = Array.from({ length: 24 }, (_, index) => agentEntry(`${['Brindle scout', 'Aster flux', 'Cobalt surveyor'][index % 3]} ${String(Math.floor(index / 3) + 1).padStart(2, '0')}`, `highlight-${index + 1}`))
/** A reposition above the painted rows: a new agent arrives first and the first two fixture agents swap. */
const insertedEntry = agentEntry('Dun courier 01', 'highlight-inserted')
const repositionedFixture: readonly AgentEntry[] = [insertedEntry, highlightFixture[1]!, highlightFixture[0]!, ...highlightFixture.slice(2)]
const entriesByKey = new Map(repositionedFixture.map(entry => [String(entry.id), entry] as const))

/** A host that filters and repositions SidebarAgentRow rows, publishing its query through SidebarQueryContext with a stable renderRow. */
function HighlightProbe() {
  const [query, setQuery] = React.useState('')
  const [rowProp, setRowProp] = React.useState(false)
  const [repositioned, setRepositioned] = React.useState(false)
  const items = React.useMemo(() => {
    const source = repositioned ? repositionedFixture : highlightFixture
    return query === '' ? source : source.filter(entry => entry.textValue.toLowerCase().includes(query.toLowerCase()))
  }, [query, repositioned])
  // Control: the per-row query prop of earlier call sites. The context then holds a decoy,
  // so the highlights also prove that the explicit prop takes precedence.
  const rowQuery = rowProp ? query : undefined
  const renderRow = React.useCallback((entry: AgentEntry) => <SidebarAgentRow item={entry.row} now={highlightNow} inTree query={rowQuery} />, [rowQuery])
  return <section data-testid="highlight-probe" {...stylex.props(styles.probe, styles.wide)}>
    <div {...stylex.props(styles.toolbar)}>
      <input aria-label="Filter highlighted agents" type="search" value={query} onChange={event => setQuery(event.currentTarget.value)} {...stylex.props(styles.input)} />
      <label><input type="checkbox" checked={rowProp} onChange={event => setRowProp(event.currentTarget.checked)} /> Query as a row prop</label>
      <button type="button" onClick={() => setRepositioned(true)}>Insert an agent above and swap the first two</button>
    </div>
    <SidebarQueryContext.Provider value={rowProp ? decoyQuery : query}>
      <div {...stylex.props(styles.viewport)}>
        <SidebarTree aria-label="Highlighted agents" items={items} expandedKeys={emptyExpanded} onExpandedChange={onExpandedChange} renderRow={renderRow} />
      </div>
    </SidebarQueryContext.Provider>
  </section>
}

const meta = {
  title: 'Fractal UI/Sidebar tree',
  component: SidebarTree,
  parameters: {
    layout: 'fullscreen',
    docs: { description: { component: 'Production SidebarTree filter probe with 78 identity-retained synthetic agents. Primary, deterministic: each keystroke may render only painted rows whose keyed DOM row changed, with no churn of item identity; a fresh-object rebuild control must reject. Secondary, calibrated: the filter keystroke-to-next-frame p90 must stay within 3x the p90 of a trivial reference keystroke in the same page, the two alternating keystroke by keystroke and pooled over the phases before and after the controls; a busy-wait control making each filter keystroke wait 4x its paired reference keystroke must reject. This kit-only probe does not measure application agent-search latency.' } },
  },
} satisfies Meta
export default meta
type Story = StoryObj

const read = (root: HTMLElement, key: 'filterSamples' | 'referenceSamples'): readonly Sample[] => JSON.parse(root.dataset[key] ?? '[]') as Sample[]
const p90 = (samples: readonly Sample[]) => [...samples].map(sample => sample.durationMs).sort((a, b) => a - b)[Math.ceil(samples.length * 0.9) - 1]!

type Inputs = { readonly reference: HTMLInputElement; readonly filter: HTMLInputElement }

// One keystroke per sample, each waiting until the previous sample has settled. Reference
// and filter keystrokes alternate so both distributions see the same host-load windows.
// With `busyControl`, each filter keystroke busy-waits `controlRatio` times the reference
// keystroke just before it, so every filter sample is at least that multiple of its pair
// and the filter p90 is at least `controlRatio` times the reference p90, whatever the load.
async function collect(root: HTMLElement, inputs: Inputs, phase: string, busyControl = false) {
  const before = { reference: read(root, 'referenceSamples').length, filter: read(root, 'filterSamples').length }
  await expect(inputs.reference.value).toBe('')
  await expect(inputs.filter.value).toBe('')
  // A settle timeout names the probe, phase, sample, typed value and committed query. The
  // message is built only on failure, so polling adds no work to the timed keystrokes.
  const settle = async (input: 'reference' | 'filter', index: number) => {
    const key = input === 'filter' ? 'filterSamples' : 'referenceSamples'
    try {
      await waitFor(() => expect(read(root, key)[before[input] + index]?.renders).toBeDefined(), settled)
    } catch (error) {
      throw new Error(`${root.dataset.testid} ${phase} ${input} sample ${index}: input "${inputs[input].value}", committed query "${root.dataset.query}", ${read(root, key).length - before[input]} samples recorded`, { cause: error })
    }
  }
  for (let index = 0; index < sampleCount; index++) {
    const toggle = index % 2 === 0
    await userEvent.type(inputs.reference, toggle ? 'r' : '{Backspace}')
    await settle('reference', index)
    if (busyControl) root.dataset.synchronousCostMs = String(controlRatio * read(root, 'referenceSamples')[before.reference + index]!.durationMs)
    await userEvent.type(inputs.filter, toggle ? 'x' : '{Backspace}')
    await settle('filter', index)
  }
  await expect(inputs.reference.value).toBe('')
  await expect(inputs.filter.value).toBe('')
  return { reference: read(root, 'referenceSamples').slice(before.reference), samples: read(root, 'filterSamples').slice(before.filter) }
}

// Primary check. Every keystroke renders only painted rows whose keyed DOM row changed
// (newly mounted or a rebound RAC view), and every rendered item is the retained fixture object.
async function assertWork(samples: readonly Sample[]) {
  await expect(samples).toHaveLength(sampleCount)
  for (const [index, sample] of samples.entries()) {
    const filtered = index % 2 === 0
    await expect(sample.value).toBe(filtered ? 'x' : '')
    await expect(sample.count).toBe(filtered ? 26 : 78)
    await expect(sample.paintedKeys[0]).toBe(filtered ? 'filter-agent-2' : 'filter-agent-1')
    await expect(sample.retainedIdentity).toBe(true)
    const renders = sample.renders!
    await expect(renders.every(call => call.retained)).toBe(true)
    for (const call of renders) await expect(sample.changedKeys).toContain(call.key)
    await expect(renders.length).toBeLessThanOrEqual(sample.changedKeys.length)
  }
}

// Secondary check, calibrated against reference keystrokes interleaved with the filter ones.
async function assertRatio(filter: readonly Sample[], reference: readonly Sample[]) {
  await expect(filter.length).toBeGreaterThanOrEqual(sampleCount)
  await expect(reference).toHaveLength(filter.length)
  await expect(p90(filter) / p90(reference)).toBeLessThanOrEqual(ratioBudget)
}

export const FilterKeystrokeToPaint: Story = {
  render: () => <main {...stylex.props(styles.matrix)}>{[false, true].flatMap(light => [true, false].map(narrow => <FilterProbe key={`${light}-${narrow}`} light={light} narrow={narrow} />))}</main>,
  play: async ({ canvasElement }) => {
    await document.fonts.ready
    // Allow initial virtualizer measurements to settle outside all samples.
    for (let index = 0; index < 6; index++) await nextFrame()
    for (const root of canvasElement.querySelectorAll<HTMLElement>('[data-testid^="filter-probe-"]')) {
      const inputs: Inputs = { filter: within(root).getByRole('searchbox') as HTMLInputElement, reference: within(root).getByRole('textbox') as HTMLInputElement }
      await expect(root.dataset.visibleCount).toBe('78')
      const phase = async (name: string, busyControl = false) => {
        const sampled = await collect(root, inputs, name, busyControl)
        root.dataset[`${name}Ratio`] = String(p90(sampled.samples) / p90(sampled.reference))
        return sampled
      }
      const production = await phase('production')
      await assertWork(production.samples)
      try {
        root.dataset.rebuildControl = 'true'
        const rebuild = await phase('rebuild')
        await expect(assertWork(rebuild.samples)).rejects.toThrow()
      } finally {
        root.dataset.rebuildControl = 'false'
      }
      try {
        const busy = await phase('busy', true)
        await expect(assertRatio(busy.samples, busy.reference)).rejects.toThrow()
      } finally {
        root.dataset.synchronousCostMs = '0'
      }
      const restored = await phase('restored')
      await assertWork(restored.samples)
      // The ratio pools both unregressed phases (60 pairs, p90 is the 54th). A control
      // left armed would make half the pool slow and so set the p90.
      const filter = [...production.samples, ...restored.samples]
      const reference = [...production.reference, ...restored.reference]
      root.dataset.pooledRatio = String(p90(filter) / p90(reference))
      await assertRatio(filter, reference)
    }
  },
}

const titleText = (row: HTMLElement) => row.querySelector<HTMLElement>('[data-row-column="title-text"]')!
/** The title as painted, each highlighted letter bracketed. */
const paintedTitle = (row: HTMLElement) => Array.from(titleText(row).childNodes, node => node.nodeName === 'MARK' ? `[${node.textContent}]` : node.textContent).join('')
const expectedTitle = (title: string, query: string) => {
  const matched = matchPositions({ query, text: title })
  return Array.from(title, (letter, index) => matched.has(index) ? `[${letter}]` : letter).join('')
}

// One keystroke: waits until every painted title carries the query's highlights, then reports,
// for each row whose keyed DOM row survived the keystroke, its SidebarAgentRow renders and
// whether its highlights changed.
async function highlightKeystroke(root: HTMLElement, input: HTMLInputElement, key: string, query: string) {
  const before = paintedRows(root)
  const titlesBefore = new Map([...before].map(([ref, row]) => [ref, paintedTitle(row)] as const))
  const reads = new Map(titleReads)
  await userEvent.type(input, key)
  await expect(input.value).toBe(query)
  await waitFor(async () => {
    for (const row of paintedRows(root).values()) await expect(paintedTitle(row)).toBe(expectedTitle(titleText(row).textContent!, query))
  }, settled)
  for (let index = 0; index < settleFrames; index++) await nextFrame()
  const after = paintedRows(root)
  const retained = [...after].filter(([ref, row]) => before.get(ref) === row).map(([ref, row]) => ({ ref, renders: (titleReads.get(ref) ?? 0) - (reads.get(ref) ?? 0), highlightChanged: paintedTitle(row) !== titlesBefore.get(ref) }))
  return { painted: after.size, retained }
}

/**
 * The host publishes its query through SidebarQueryContext. A keystroke re-renders only the
 * highlighted titles: rows that stay painted keep their DOM row and never re-render. A control
 * passing the query as a row prop must re-render them, with the prop overriding the context.
 */
export const FilterHighlightsWithoutRowRenders: Story = {
  render: () => <main {...stylex.props(styles.matrix)}><HighlightProbe /></main>,
  play: async ({ canvasElement }) => {
    await document.fonts.ready
    for (let index = 0; index < 6; index++) await nextFrame()
    const root = within(canvasElement).getByTestId('highlight-probe')
    const input = within(root).getByRole('searchbox') as HTMLInputElement
    // '' to 's' keeps every row and highlights each; 'su' keeps only the Cobalt rows; the
    // backspaces remount the others, and the last one clears every highlight.
    const keep = await highlightKeystroke(root, input, 's', 's')
    await expect(keep.retained.length).toBe(keep.painted)
    for (const row of keep.retained) await expect(row).toEqual({ ref: row.ref, renders: 0, highlightChanged: true })
    for (const [key, query] of [['u', 'su'], ['{Backspace}', 's'], ['{Backspace}', '']] as const) {
      const step = await highlightKeystroke(root, input, key, query)
      await expect(step.retained.length).toBeGreaterThan(0)
      await expect(step.retained.some(row => row.highlightChanged)).toBe(true)
      for (const row of step.retained) await expect(row.renders).toBe(0)
    }
    const control = within(root).getByRole('checkbox', { name: 'Query as a row prop' })
    await userEvent.click(control)
    try {
      const step = await highlightKeystroke(root, input, 's', 's')
      await expect(step.retained.length).toBe(step.painted)
      for (const row of step.retained) await expect(row.renders).toBeGreaterThan(0)
    } finally {
      await userEvent.click(control)
    }
  },
}

// Every accessible row carries its item's exact name, and every description it references is mounted.
async function assertAgentRowNames(tree: HTMLElement) {
  const rows = within(tree).getAllByRole('row')
  await expect(rows.length).toBeGreaterThan(0)
  for (const row of rows) {
    await expect(row).toHaveAccessibleName(entriesByKey.get(row.dataset.sidebarTreeKey!)!.accessibleName!)
    for (const described of row.querySelectorAll('[aria-describedby]')) for (const id of described.getAttribute('aria-describedby')!.split(/\s+/)) await expect(document.getElementById(id)?.isConnected).toBe(true)
  }
}

/**
 * An agent arrives above the painted rows and the first two swap. Repositioned rows keep their
 * keyed DOM row and their memoized content never re-renders, yet each row's accessible name is
 * exactly its item's `accessibleName`, because RAC names rows from item props.
 */
export const AgentRowNamesSurviveReposition: Story = {
  render: () => <main {...stylex.props(styles.matrix)}><HighlightProbe /></main>,
  play: async ({ canvasElement }) => {
    await document.fonts.ready
    for (let index = 0; index < 6; index++) await nextFrame()
    const root = within(canvasElement).getByTestId('highlight-probe')
    const tree = within(root).getByRole('treegrid')
    await assertAgentRowNames(tree)
    const before = paintedRows(root)
    const reads = new Map(titleReads)
    await userEvent.click(within(root).getByRole('button', { name: 'Insert an agent above and swap the first two' }))
    const top = (entry: AgentEntry) => paintedRows(root).get(String(entry.id))?.getBoundingClientRect().top ?? Number.NaN
    await waitFor(async () => {
      await expect(top(insertedEntry)).toBeLessThan(top(highlightFixture[1]!))
      await expect(top(highlightFixture[1]!)).toBeLessThan(top(highlightFixture[0]!))
      await assertAgentRowNames(tree)
    }, settled)
    for (let index = 0; index < settleFrames; index++) await nextFrame()
    await assertAgentRowNames(tree)
    const retained = [...paintedRows(root)].filter(([key, row]) => before.get(key) === row).map(([key]) => key)
    await expect(retained).toEqual(expect.arrayContaining([String(highlightFixture[0]!.id), String(highlightFixture[1]!.id)]))
    for (const key of retained) await expect((titleReads.get(key) ?? 0) - (reads.get(key) ?? 0)).toBe(0)
  },
}

const styles = stylex.create({
  matrix: { display: 'flex', flexWrap: 'wrap', alignItems: 'flex-start', gap: s.xl, padding: s.xl, backgroundColor: surface.canvas },
  probe: { minWidth: 0, color: ink.fg, backgroundColor: surface.sidebar, fontFamily: t.fontSans, fontSize: t.metaSize },
  narrow: { width: sizes.narrow },
  wide: { width: sizes.small },
  toolbar: { display: 'flex', flexDirection: 'column', gap: s.sm, padding: s.md },
  input: { boxSizing: 'border-box', width: '100%', minWidth: 0, height: g.controlLg, paddingInline: s.md, color: ink.fg, backgroundColor: surface.controlFill, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, fontFamily: t.fontSans, fontSize: t.uiSize, outlineColor: ink.fgMuted },
  viewport: { height: sizes.viewport, minHeight: 0 },
  row: { display: 'flex', alignItems: 'center', height: sizes.row, paddingInline: s.sm, overflow: 'hidden', whiteSpace: 'nowrap', textOverflow: 'ellipsis', minWidth: 0 },
})
