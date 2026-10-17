import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { expect, userEvent, waitFor, within } from 'storybook/test'
import { Button } from 'react-aria-components'
import { Workbench, type WorkbenchProps } from './assistant-ui/workbench/Workbench'
import { paneTabIdentity } from './assistant-ui/workbench/TabStrip'
import { group, paneKey, parsePaneKey, split, type WorkbenchPane } from './assistant-ui/workbench/workbench-model'
import { detailsWorkbenchResources as workbenchResources } from './assistant-ui/details/story-fixtures'
import { DetailsPane, type DetailsLayoutIntent } from './assistant-ui/details/DetailsPane'
import { DetailsLabelContext } from './assistant-ui/details/DetailsResources'
import { detailsStoryInputs } from './assistant-ui/details/story-fixtures'
import { baselineTheme } from './assistant-ui/neutral-theme'
import { lightTheme } from './assistant-ui/composition-theme'
import { ThemePortal } from './assistant-ui/taste/ThemePortal'
import { surfaceVars as surface, textVars as text, spaceVars as space, typeVars as type, radiusVars as radius } from './assistant-ui/composition-tokens.stylex'

const panes: readonly WorkbenchPane[] = [
  { uri: 'agent:worker-1', view: 'details' },
  { uri: 'agent:worker-1' },
  { uri: 'agent:worker-2' },
  { uri: 'diff:sample/rows.ts', form: 'unified' },
  { uri: 'terminal:build-shell' },
  { uri: 'directory:src/components' },
  { uri: 'link:https://example.com/release-notes' },
  { uri: 'file:docs/release-checklist.md' },
]
const names = ['Details', 'Release coordinator', 'Accessibility review', 'rows.ts', 'Build shell', 'components', 'Release notes', 'release-checklist.md']
interface Args extends WorkbenchProps { mountedWidth: number; comparison: boolean }
let initialShifts: Shift[] = []
let initialObserver: PerformanceObserver | undefined
const styles = stylex.create({
  root: { display: 'flex', flexDirection: 'column', height: 520, minHeight: 0, overflow: 'hidden', backgroundColor: surface.canvas },
  comparison: { display: 'flex', flexWrap: 'wrap', gap: 16, backgroundColor: surface.canvas },
  controls: { display: 'flex', flexShrink: 0, alignItems: 'center' },
  control: { color: text.fgMuted, backgroundColor: surface.transparent, borderWidth: 0, borderRadius: radius.control, paddingInline: space.sm, paddingBlock: space.xs, fontFamily: type.fontSans, fontSize: type.metaSize, cursor: 'pointer' },
})

/** Only production components are mounted; the CSF render callback owns host observations. */
const meta = {
  title: 'Fractal UI/Details Tab Strip',
  component: Workbench,
  parameters: {
    layout: 'fullscreen',
    docs: { description: { component: 'Production Workbench with deterministic thread and diff resources and the real Details pane. Terminal, directory, link and document tabs exercise resource identities through Workbench’s existing placeholder surface: they do not claim available renderer bodies, live terminal sessions or loaded document contents.' } },
  },
  beforeEach: async () => {
    await document.fonts.ready
    initialShifts = []
    initialObserver = undefined
    if (!PerformanceObserver.supportedEntryTypes.includes('layout-shift')) return
    const observer = new PerformanceObserver(list => initialShifts.push(...list.getEntries() as Shift[]))
    initialObserver = observer
    observer.observe({ type: 'layout-shift', buffered: false })
    return () => observer.disconnect()
  },
  args: {
    resources: workbenchResources,
    layout: group(panes),
    workspaceId: 'details-tab-strip',
    scheme: 'dark',
    mountedWidth: 800,
    comparison: false,
    describePane: pane => ({ title: names[panes.findIndex(item => paneKey(item) === paneKey(pane))] ?? pane.uri }),
  },
  render: function Render({ mountedWidth, comparison, ...args }) {
    const [layout, setLayout] = React.useState(args.layout)
    const [focused, setFocused] = React.useState(args.focusedPaneKey)
    const [intent, setIntent] = React.useState<DetailsLayoutIntent>({ hiddenSections: [] })
    const [state, setState] = React.useState<'busy' | 'loading' | 'error'>('busy')
    const renderPane = (pane: WorkbenchPane) => pane.view === 'details' ? <DetailsPane
      {...detailsStoryInputs(state)}
      variants={{ overview: 'O2', context: 'C2', spend: 'S1', resources: 'R2', empty: 'E3' }}
      syncStatus={state === 'error' ? { _tag: 'Failed', cause: { _tag: 'Server', code: 'timeout', message: 'The agent snapshot request timed out.' } } : state === 'loading' ? { _tag: 'Connecting', attempt: 1, since: 1799999999000 } : { _tag: 'Live', since: 1799999999000 }}
      state={state} intent={intent} onIntentChange={setIntent}
      onOpen={(key, asSplit) => { setLayout(current => asSplit ? split('right', current, group([parsePaneKey(key)])) : current.kind === 'group' ? group([...current.tabs, parsePaneKey(key)]) : current); setFocused(key) }}
      onRetry={() => setState('busy')}
    /> : args.renderPane?.(pane)
    const cases = comparison ? (['dark', 'light'] as const).flatMap(scheme => [159, 160, 800].map(width => ({ scheme, width }))) : [{ scheme: args.scheme, width: mountedWidth }]
    // AllStates renders several stages, so each is a labelled region rather than a duplicate main landmark.
    return <div {...stylex.props(comparison && styles.comparison)}>{cases.map(({ scheme, width }) => <section key={`${scheme}-${width}`} aria-label={`${scheme} ${width}px`} data-testid="tab-strip-stage" style={{ width }} {...stylex.props(styles.root, ...baselineTheme, ...(scheme === 'light' ? lightTheme : []))}>
      <header {...stylex.props(styles.controls)}><Button {...stylex.props(styles.control)} onPress={() => setState('loading')}>Loading details</Button><Button {...stylex.props(styles.control)} onPress={() => setState('error')}>Fail details</Button></header>
      <DetailsLabelContext.Provider value={`${scheme} ${width}px `}><ThemePortal><Workbench {...args} scheme={scheme} workspaceId={`${args.workspaceId}-${scheme}-${width}`} layout={layout} focusedPaneKey={focused} onPaneSelect={setFocused} onLayoutChange={setLayout} renderPane={renderPane} /></ThemePortal></DetailsLabelContext.Provider>
    </section>)}</div>
  },
} satisfies Meta<Args>
export default meta
type Story = StoryObj<typeof meta>

async function frames(count = 3) {
  for (let index = 0; index < count; index++) await new Promise<void>(resolve => requestAnimationFrame(() => resolve()))
}
function requireContract(condition: boolean, reason: string): asserts condition {
  if (!condition) throw new Error(reason)
}
async function rejectsControl(reason: string, assertion: () => void | Promise<void>) {
  let rejected = false
  try { await assertion() } catch (error) {
    requireContract(error instanceof Error && error.message === reason, `Unexpected control failure: ${String(error)}`)
    rejected = true
  }
  requireContract(rejected, `Negative control did not reject: ${reason}`)
}
type Box = { left: number; right: number; top: number; bottom: number }
function intersect(a: Box, b: Box): Box {
  return { left: Math.max(a.left, b.left), right: Math.min(a.right, b.right), top: Math.max(a.top, b.top), bottom: Math.min(a.bottom, b.bottom) }
}
function effectiveClip(strip: HTMLElement): Box {
  const rect = strip.getBoundingClientRect()
  let clip: Box = { left: rect.left + strip.clientLeft, right: rect.left + strip.clientLeft + strip.clientWidth, top: rect.top + strip.clientTop, bottom: rect.top + strip.clientTop + strip.clientHeight }
  for (let parent = strip.parentElement; parent; parent = parent.parentElement) {
    const css = getComputedStyle(parent)
    const box = parent.getBoundingClientRect()
    if (/(hidden|clip|auto|scroll)/.test(css.overflowX)) clip = intersect(clip, { left: box.left + parent.clientLeft, right: box.left + parent.clientLeft + parent.clientWidth, top: -Infinity, bottom: Infinity })
    if (/(hidden|clip|auto|scroll)/.test(css.overflowY)) clip = intersect(clip, { left: -Infinity, right: Infinity, top: box.top + parent.clientTop, bottom: box.top + parent.clientTop + parent.clientHeight })
  }
  return clip
}
function painted(element: Element): boolean {
  for (let current: Element | null = element; current; current = current.parentElement) {
    const css = getComputedStyle(current)
    if (css.display === 'none' || css.visibility === 'hidden' || Number(css.opacity) === 0) return false
  }
  return true
}
/** Independent paint oracle: transform each actual SVG graphics element's geometry and stroke into viewport coordinates. */
function assertPaint(strip: HTMLElement) {
  const clip = effectiveClip(strip)
  for (const tab of strip.querySelectorAll<HTMLElement>('[role="tab"]')) {
    const icon = tab.querySelector<HTMLElement>('[data-tab-icon]')!
    const label = tab.querySelector<HTMLElement>('[data-tab-label]')!
    requireContract(Boolean(icon && label), 'Missing stable representation slot')
    const active = strip.dataset.tabMode === 'icons' ? icon : label
    if (!painted(active)) continue
    requireContract(strip.dataset.tabMode === 'icons' || strip.dataset.tabMode === 'labels', 'Missing active representation')
    const boxes: Box[] = []
    if (strip.dataset.tabMode === 'icons') {
      for (const graphic of active.querySelectorAll<SVGGraphicsElement>('path, line, polyline, polygon, circle, ellipse, rect')) {
        const matrix = graphic.getScreenCTM()
        requireContract(matrix !== null, 'Missing painted SVG transform')
        const box = graphic.getBBox()
        const css = getComputedStyle(graphic)
        const stroke = css.stroke === 'none' ? 0 : Number.parseFloat(css.strokeWidth) / 2
        const points = [[box.x - stroke, box.y - stroke], [box.x + box.width + stroke, box.y - stroke], [box.x - stroke, box.y + box.height + stroke], [box.x + box.width + stroke, box.y + box.height + stroke]].map(([x, y]) => new DOMPoint(x, y).matrixTransform(matrix))
        boxes.push({ left: Math.min(...points.map(point => point.x)), right: Math.max(...points.map(point => point.x)), top: Math.min(...points.map(point => point.y)), bottom: Math.max(...points.map(point => point.y)) })
      }
      requireContract(boxes.length > 0, 'Missing painted icon')
    } else {
      // CSS ellipsis clips intrinsic text; its visible paint cannot exceed the label box.
      boxes.push(active.getBoundingClientRect())
      const minimum = tab.querySelector<HTMLElement>('[data-tab-minimum]')!.getBoundingClientRect().width
      requireContract(active.getBoundingClientRect().width >= minimum, 'Label lacks meaningful prefix and ellipsis')
      const text = active.firstChild!
      let offset = 0
      for (const character of Array.from(tab.getAttribute('aria-label')!).slice(0, 4)) {
        const range = tab.ownerDocument.createRange()
        range.setStart(text, offset)
        offset += character.length
        range.setEnd(text, offset)
        boxes.push(range.getBoundingClientRect())
      }
    }
    const epsilon = Number.EPSILON * Math.max(1, Math.abs(clip.left), Math.abs(clip.right), Math.abs(clip.bottom)) * 8
    for (const box of boxes) requireContract(box.left >= clip.left - epsilon && box.right <= clip.right + epsilon && box.top >= clip.top - epsilon && box.bottom <= clip.bottom + epsilon, 'Partial representation paint')
  }
}
async function stripFor(canvasElement: HTMLElement) {
  await canvasElement.ownerDocument.fonts.ready
  await frames()
  const strip = canvasElement.querySelector<HTMLElement>('[data-tab-strip]')
  requireContract(strip !== null, 'Missing production tab strip')
  return strip
}
async function containmentPlay({ canvasElement }: { canvasElement: HTMLElement }) {
  const strip = await stripFor(canvasElement)
  const stage = within(canvasElement).getByTestId('tab-strip-stage')
  const originalWidth = stage.style.width
  const initiallySelected = within(strip).getByRole('tab', { selected: true })
  assertSelected(initiallySelected)
  const workbench = within(stage).getByTestId('workbench')
  const originalWorkbenchWidth = workbench.style.width
  try {
    // Canonical states stay quick; scripts/tab-strip-interactions.mjs sweeps every
    // integer width and offset independently of this play and the layout policy.
    for (const width of [118, 159, 160, 240, 320, 359.5, 360.25, 375.5, 376.25, 520, 640, 800]) {
      stage.style.width = `${width}px`
      workbench.style.width = `${width}px`
      await frames(1)
      assertPaint(strip)
      const max = strip.scrollWidth - strip.clientWidth
      for (const offset of [0, 0.25, 1, max / 2, Math.max(0, max - 0.25), max]) {
        strip.scrollLeft = offset
        strip.dispatchEvent(new Event('scroll'))
        assertPaint(strip)
        await frames(1)
        assertPaint(strip)
      }
      if (width === 800) {
        requireContract(strip.dataset.tabMode === 'labels', 'Wide fixture did not restore labels')
        const label = [...strip.querySelectorAll<HTMLElement>('[data-tab-label]')].find(painted)!
        const original = label.style.width
        label.style.width = '1px'
        try { await rejectsControl('Label lacks meaningful prefix and ellipsis', () => assertPaint(strip)) } finally { label.style.width = original }
        assertPaint(strip)
      }
      if (width === 159 || width === 160) {
        strip.scrollLeft = 0
        strip.dispatchEvent(new Event('scroll'))
        await frames()
        const svg = [...strip.querySelectorAll<SVGSVGElement>('[data-tab-icon] svg')].find(painted)
        requireContract(svg !== undefined, 'Narrow strip paints no icon')
        const transform = svg.style.transform
        svg.style.transform = `translateX(${effectiveClip(strip).right - svg.getBoundingClientRect().left + 1}px)`
        try { await rejectsControl('Partial representation paint', () => assertPaint(strip)) } finally { svg.style.transform = transform }
        assertPaint(strip)
      }
    }
  } finally { stage.style.width = originalWidth; workbench.style.width = originalWorkbenchWidth }
  await frames()
  assertPaint(strip)
}
function assertSelected(tab: HTMLElement) {
  requireContract(tab.getAttribute('aria-selected') === 'true', 'Wrong selected resource')
  const panelId = tab.getAttribute('aria-controls')
  requireContract(Boolean(panelId && tab.ownerDocument.getElementById(panelId)), 'Missing selected panel relationship')
  const panel = tab.ownerDocument.getElementById(panelId!)!
  requireContract(panel.querySelector<HTMLElement>('[data-pane-key]')?.dataset.paneKey === tab.dataset.tabKey, 'Wrong resource body')
  requireContract(painted(tab), 'Selected resource is not revealed whole')
}
/**
 * The tab's disclosure, resolved through its own aria-describedby. A document-wide ByRole
 * query re-runs on every mutation at 46ms (unthrottled) to 319ms (6x CPU) per check on this
 * page, so under load a few checks consumed the whole 1s budget meant for RAC's 0 or 350ms
 * (warm or cold) open delay and missed a tooltip that was already mounted.
 */
async function tooltipFor(tab: HTMLElement, name: string) {
  return waitFor(() => {
    const tooltip = tab.getAttribute('aria-describedby')?.split(/\s+/).map(id => tab.ownerDocument.getElementById(id)).find(node => node?.getAttribute('role') === 'tooltip')
    requireContract(tooltip !== undefined && tooltip !== null, `Missing tooltip for ${name}`)
    expect(tooltip).toHaveAccessibleName(name)
    return tooltip
  })
}
async function interactionPlay({ canvasElement }: { canvasElement: HTMLElement }) {
  const strip = await stripFor(canvasElement)
  const tabs = within(strip).getAllByRole('tab')
  tabs.forEach((tab, index) => expect(tab).toHaveAccessibleName(names[index]!))
  tabs.forEach((tab, index) => expect(tab).toHaveAttribute('data-tab-kind', paneTabIdentity(panes[index]!).kind))
  const glyphs = tabs.map(tab => tab.querySelector('[data-tab-icon] svg')!.innerHTML)
  requireContract(glyphs[1] === glyphs[2], 'Same-kind glyph identity changed')
  requireContract(new Set(glyphs.filter((_, index) => index !== 2)).size === 7, 'Resource kind glyphs are not distinct')
  const threadGlyph = tabs[1].querySelector('[data-tab-icon] svg')!
  threadGlyph.innerHTML = glyphs[0]!
  try { await rejectsControl('Same-kind glyph identity changed', () => requireContract(threadGlyph.innerHTML === glyphs[2], 'Same-kind glyph identity changed')) } finally { threadGlyph.innerHTML = glyphs[1]! }
  const identity = [...tabs]
  for (let step = 0; canvasElement.ownerDocument.activeElement !== tabs[0] && step < 100; step++) await userEvent.tab()
  expect(tabs[0]).toHaveFocus()
  await userEvent.keyboard('{End}')
  expect(tabs.at(-1)).toHaveFocus()
  assertSelected(tabs.at(-1)!)
  assertPaint(strip)
  await userEvent.keyboard('{Home}{ArrowRight}{ArrowRight}{ArrowLeft}')
  expect(tabs[1]).toHaveFocus()
  assertSelected(tabs[1]!)
  const focusedTooltip = await tooltipFor(tabs[1]!, names[1]!)
  expect(focusedTooltip).toBeVisible()
  expect(tabs[1].getAttribute('aria-describedby')?.split(/\s+/)).toContain(focusedTooltip.id)
  await userEvent.keyboard('{Escape}')
  await waitFor(() => expect(focusedTooltip).not.toBeInTheDocument())
  expect(tabs[1]).toHaveFocus()
  const assertFocus = () => requireContract(tabs[1].hasAttribute('data-focus-visible') && getComputedStyle(tabs[1]).outlineStyle !== 'none' && Number.parseFloat(getComputedStyle(tabs[1]).outlineWidth) > 0, 'Missing keyboard focus ring')
  assertFocus()
  const outline = tabs[1].style.outlineStyle
  tabs[1].style.outlineStyle = 'none'
  try { await rejectsControl('Missing keyboard focus ring', assertFocus) } finally { tabs[1].style.outlineStyle = outline }
  for (const tab of [tabs[2], tabs.at(-1)!, tabs[0]]) {
    await userEvent.keyboard(tab === tabs[2] ? '{Home}{ArrowRight}{ArrowRight}' : tab === tabs[0] ? '{Home}' : '{End}')
    await userEvent.click(tab)
    assertSelected(tab)
    assertPaint(strip)
    const selected = tab.getAttribute('aria-selected')!
    tab.setAttribute('aria-selected', 'false')
    try { await rejectsControl('Wrong selected resource', () => assertSelected(tab)) } finally { tab.setAttribute('aria-selected', selected) }
    // RAC closes hover disclosure on press; leave before exercising a fresh hover.
    await userEvent.unhover(tab)
    await userEvent.hover(tab)
    const name = tab.getAttribute('aria-label')!
    const tooltip = await tooltipFor(tab, name)
    await waitFor(() => expect(tooltip).toBeVisible())
    const assertTooltip = () => requireContract(Boolean(tab.getAttribute('aria-describedby')?.split(/\s+/).includes(tooltip.id)) && tooltip.getBoundingClientRect().top >= tab.getBoundingClientRect().bottom, 'Missing anchored full-name tooltip')
    assertTooltip()
    const described = tab.getAttribute('aria-describedby')!
    tab.removeAttribute('aria-describedby')
    try { await rejectsControl('Missing anchored full-name tooltip', assertTooltip) } finally { tab.setAttribute('aria-describedby', described) }
    const active = canvasElement.ownerDocument.activeElement
    await userEvent.keyboard('{Escape}')
    await waitFor(() => expect(tooltip).not.toBeInTheDocument())
    expect(canvasElement.ownerDocument.activeElement).toBe(active)
    await userEvent.unhover(tab)
  }
  expect(within(strip).getAllByRole('tab')).toEqual(identity)
}
async function thresholdPlay({ canvasElement }: { canvasElement: HTMLElement }) {
  const strip = await stripFor(canvasElement)
  const assertThreshold = () => {
    const width = Number(strip.dataset.tabMeasuredWidth)
    const collapse = Number(strip.dataset.tabCollapseBelow)
    const expand = Number(strip.dataset.tabExpandAt)
    requireContract(Number.isFinite(width) && collapse >= 360 && expand === collapse + 16, 'Incorrect measured threshold')
    if (width < collapse) requireContract(strip.dataset.tabMode === 'icons', 'Incorrect threshold mode')
    if (width >= expand) requireContract(strip.dataset.tabMode === 'labels', 'Incorrect threshold mode')
  }
  assertThreshold()
  const expand = strip.dataset.tabExpandAt!
  strip.dataset.tabExpandAt = strip.dataset.tabCollapseBelow
  try { await rejectsControl('Incorrect measured threshold', assertThreshold) } finally { strip.dataset.tabExpandAt = expand }
  assertThreshold()
  const stage = within(canvasElement).getByTestId('tab-strip-stage')
  const originalWidth = stage.style.width
  const reserved = stage.getBoundingClientRect().width - strip.getBoundingClientRect().width
  // These two short prefix probes fit far below the approved floor: the independently expected boundary is 360/376.
  requireContract([...strip.querySelectorAll<HTMLElement>('[data-tab-minimum]')].every(probe => probe.getBoundingClientRect().width < 100), 'Threshold fixture exceeds minimum budget')
  requireContract(Number(strip.dataset.tabCollapseBelow) === 360, 'Incorrect measured threshold')
  try {
    let previous: 'icons' | 'labels' = 'icons'
    for (const width of [359, 360, 375.5, 376, 375.5, 360, 359.75, 360, 375.75, 376]) {
      stage.style.width = `${width + reserved}px`
      await frames()
      const measured = strip.getBoundingClientRect().width
      previous = previous === 'icons' ? measured >= 376 ? 'labels' : 'icons' : measured < 360 ? 'icons' : 'labels'
      requireContract(strip.dataset.tabMode === previous, 'Incorrect hysteresis history')
      assertThreshold()
      assertPaint(strip)
    }
    const mode = strip.dataset.tabMode!
    strip.dataset.tabMode = 'icons'
    try { await rejectsControl('Incorrect threshold mode', assertThreshold) } finally { strip.dataset.tabMode = mode }
  } finally { stage.style.width = originalWidth }
  await containmentPlay({ canvasElement })
}
interface Shift extends PerformanceEntry { value: number; hadRecentInput: boolean; sources?: readonly { node?: Node; previousRect: DOMRectReadOnly; currentRect: DOMRectReadOnly }[] }
async function stabilityPlay({ canvasElement }: { canvasElement: HTMLElement }) {
  const strip = await stripFor(canvasElement)
  requireContract(PerformanceObserver.supportedEntryTypes.includes('layout-shift'), 'LayoutShift observer unavailable')
  if (initialObserver) {
    initialShifts.push(...initialObserver.takeRecords() as Shift[])
    initialObserver.disconnect()
  }
  // Resource-body initialization is measured separately: transcript/follow geometry
  // is inherited, while this gate owns Details, strip anchors and recovery.
  const resourceEntries: Shift[] = []
  const resourceObserver = new PerformanceObserver(list => resourceEntries.push(...list.getEntries() as Shift[]))
  resourceObserver.observe({ type: 'layout-shift', buffered: false })
  try {
    await interactionPlay({ canvasElement })
    await frames(5)
    resourceEntries.push(...resourceObserver.takeRecords() as Shift[])
  } finally { resourceObserver.disconnect() }
  canvasElement.dataset.resourceLayoutShiftSum = String(resourceEntries.reduce((total, entry) => total + entry.value, 0))
  canvasElement.dataset.resourceLayoutShiftEntries = JSON.stringify(resourceEntries.map(entry => ({ value: entry.value, hadRecentInput: entry.hadRecentInput, sources: entry.sources?.map(source => ({ previous: source.previousRect.toJSON(), current: source.currentRect.toJSON() })) })))
  const entries = [...initialShifts]
  const observer = new PerformanceObserver(list => entries.push(...list.getEntries() as Shift[]))
  observer.observe({ type: 'layout-shift', buffered: false })
  try {
    const canvas = within(canvasElement)
    const selectedTab = within(strip).getByRole('tab', { name: 'Details', selected: true })
    const headerBox = strip.closest<HTMLElement>('[data-pane-header]')!.getBoundingClientRect()
    for (const key of ['{End}', '{Home}']) {
      await userEvent.keyboard(key)
      const selected = within(strip).getByRole('tab', { selected: true })
      assertSelected(selected)
      assertPaint(strip)
      const tooltip = await tooltipFor(selected, selected.getAttribute('aria-label')!)
      expect(tooltip).toBeVisible()
      await userEvent.keyboard('{Escape}')
      await waitFor(() => expect(tooltip).not.toBeInTheDocument())
    }
    await userEvent.click(canvas.getByRole('button', { name: 'Loading details' }))
    await userEvent.click(canvas.getByRole('button', { name: 'Fail details' }))
    const pane = await canvas.findByTestId('details-pane')
    const retry = await within(pane).findByRole('button', { name: 'Retry' })
    await userEvent.click(retry)
    await waitFor(() => {
      expect(pane).toHaveAttribute('data-details-sync-state', 'Live')
      expect(selectedTab).toHaveFocus()
    })
    expect(within(strip).getByRole('tab', { name: 'Details', selected: true })).toBe(selectedTab)
    const currentHeader = strip.closest<HTMLElement>('[data-pane-header]')!.getBoundingClientRect()
    await frames(5)
    entries.push(...observer.takeRecords() as Shift[])
    const rawSum = entries.reduce((total, entry) => total + entry.value, 0)
    canvasElement.dataset.rawLayoutShiftEntries = JSON.stringify(entries.map(entry => ({ value: entry.value, hadRecentInput: entry.hadRecentInput, sources: entry.sources?.map(source => ({ node: source.node instanceof Element ? source.node.outerHTML.slice(0, 600) : source.node?.nodeName, previous: source.previousRect.toJSON(), current: source.currentRect.toJSON() })) })))
    canvasElement.dataset.rawLayoutShiftSum = String(rawSum)
    requireContract(currentHeader.left === headerBox.left && currentHeader.top === headerBox.top && currentHeader.width === headerBox.width && currentHeader.height === headerBox.height, `Recovery moved reserved header: ${JSON.stringify({ before: headerBox.toJSON(), after: currentHeader.toJSON() })}`)
    requireContract(rawSum === 0, 'Nonzero raw layout shift')
    const margin = strip.style.marginTop
    strip.style.marginTop = '12px'
    await frames(5)
    entries.push(...observer.takeRecords() as Shift[])
    canvasElement.dataset.rawLayoutShiftControlEntries = JSON.stringify(entries.map(entry => ({ value: entry.value, hadRecentInput: entry.hadRecentInput, sources: entry.sources?.map(source => ({ previous: source.previousRect.toJSON(), current: source.currentRect.toJSON() })) })))
    try { await rejectsControl('Nonzero raw layout shift', () => requireContract(entries.reduce((total, entry) => total + entry.value, 0) === 0, 'Nonzero raw layout shift')) } finally { strip.style.marginTop = margin }
  } finally { observer.disconnect() }
}

// Every story runs in both schemes through the kit gate's `scheme` arg, so stories vary only state.
export const Wide: Story = { play: interactionPlay }
export const Threshold: Story = { args: { layout: group(panes.slice(0, 2)), mountedWidth: 376 }, play: thresholdPlay }
export const Mounted159SelectedFirst: Story = { args: { mountedWidth: 159, focusedPaneKey: paneKey(panes[0]!) }, play: containmentPlay }
export const Mounted160SelectedFirst: Story = { args: { mountedWidth: 160, focusedPaneKey: paneKey(panes[0]!) }, play: containmentPlay }
export const Mounted159SelectedLast: Story = { args: { mountedWidth: 159, focusedPaneKey: paneKey(panes.at(-1)!) }, play: containmentPlay }
export const Mounted160SelectedLast: Story = { args: { mountedWidth: 160, focusedPaneKey: paneKey(panes.at(-1)!) }, play: containmentPlay }
export const ManyTabs: Story = { args: { mountedWidth: 240 }, play: interactionPlay }
export const RawStability: Story = { play: stabilityPlay }
export const RawStability159: Story = { args: { mountedWidth: 159 }, play: stabilityPlay }
export const RawStability160: Story = { args: { mountedWidth: 160 }, play: stabilityPlay }
export const AllStates: Story = {
  args: { comparison: true },
  play: async ({ canvasElement }) => {
    await canvasElement.ownerDocument.fonts.ready
    await frames()
    for (const strip of canvasElement.querySelectorAll<HTMLElement>('[data-tab-strip]')) {
      assertPaint(strip)
      const tab = within(strip).getByRole('tab', { name: 'Details' })
      const name = tab.getAttribute('aria-label')!
      const assertName = () => requireContract(tab.getAttribute('aria-label') === 'Details', 'Missing full accessible name')
      tab.removeAttribute('aria-label')
      try { await rejectsControl('Missing full accessible name', assertName) } finally { tab.setAttribute('aria-label', name) }
      assertName()
    }
  },
}
