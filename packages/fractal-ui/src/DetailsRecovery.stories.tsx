import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { expect, userEvent, waitFor, within } from 'storybook/test'
import { Button } from './assistant-ui/composition/Controls'
import { Workbench, type WorkbenchProps } from './assistant-ui/workbench/Workbench'
import { group, paneKey, type WorkbenchPane } from './assistant-ui/workbench/workbench-model'
import { detailsWorkbenchResources as workbenchResources } from './assistant-ui/details/story-fixtures'
import { DetailsPane, type DetailsLayoutIntent, type DetailsVariants } from './assistant-ui/details/DetailsPane'
import { makeDetailsFixture } from './assistant-ui/details/fixtures'
import type { DetailsState } from './assistant-ui/details/model'
import { detailsStoryInputs } from './assistant-ui/details/story-fixtures'
import type { SyncStatus } from './assistant-ui/st3-views/sync-status'
import { baselineTheme } from './assistant-ui/neutral-theme'
import { lightTheme } from './assistant-ui/composition-theme'
import { ThemePortal } from './assistant-ui/taste/ThemePortal'
import { surfaceVars as surface } from './assistant-ui/composition-tokens.stylex'

const details: WorkbenchPane = { uri: 'agent:worker-1', view: 'details' }
const layout = group([details])
const widths = [118, 160, 240, 320, 400, 520] as const
const states = ['busy', 'partial', 'new', 'error', 'loading', 'idle'] as const
const variants: DetailsVariants[] = (['O1', 'O2', 'O3'] as const).flatMap(overview => (['C1', 'C2', 'C3'] as const).flatMap(context => (['S1', 'S2', 'S3'] as const).map(spend => ({ overview, context, spend, resources: 'R2', empty: 'E3' } satisfies DetailsVariants))))
const representative = variants.filter(variant => ['O1C1S1', 'O2C2S1', 'O2C2S2', 'O3C3S3'].includes(variant.overview + variant.context + variant.spend))
const defaultVariant = variants.findIndex(variant => variant.overview === 'O2' && variant.context === 'C2' && variant.spend === 'S1')
const cause = 'st request timed out after 15 s'
const live: SyncStatus = { _tag: 'Live', since: 1799999999000 }
const connecting: SyncStatus = { _tag: 'Connecting', attempt: 1, since: 1799999999000 }
const failed: SyncStatus = { _tag: 'Failed', cause: { _tag: 'Server', code: 'timeout', message: cause } }
type Observation = { content: DetailsState; sync: SyncStatus }
const observationFor = (content: DetailsState): Observation => ({ content, sync: content === 'error' ? failed : content === 'loading' ? connecting : live })
interface Args extends WorkbenchProps { mountedWidth: number; variantIndex: number; initialState: DetailsState }
const styles = stylex.create({
  root: { backgroundColor: surface.canvas },
  controls: { display: 'flex', height: 32, overflowX: 'auto', whiteSpace: 'nowrap' },
  stage: { display: 'flex', height: 520, minHeight: 0, overflow: 'hidden' },
})

const meta = {
  title: 'Fractal UI/Details Recovery',
  component: Workbench,
  parameters: { layout: 'fullscreen', docs: { description: { component: 'Mounted production Details recovery. Host controls change decoded observations, content variants and actual pane widths. Plays retain the inherited pending geometry, header, keyboard and accessibility batteries; schemes are args and viewports are owned by the canonical browser matrix.' } } },
  args: { resources: workbenchResources, layout, workspaceId: 'details-recovery', scheme: 'dark', mountedWidth: 320, variantIndex: defaultVariant, initialState: 'busy', describePane: () => ({ title: 'Details' }) },
  render: function Render({ mountedWidth, variantIndex, initialState, ...args }) {
    const [width, setWidth] = React.useState(mountedWidth)
    const [variant, setVariant] = React.useState(variantIndex)
    const [observation, setObservation] = React.useState<Observation>(() => observationFor(initialState))
    const [generation, setGeneration] = React.useState(0)
    const [intent, setIntent] = React.useState<DetailsLayoutIntent>({ hiddenSections: [] })
    const recover = () => setObservation(observationFor('busy'))
    const reset = (content: DetailsState) => { setObservation(observationFor(content)); setIntent({ hiddenSections: [] }); setGeneration(value => value + 1) }
    return <div {...stylex.props(styles.root, ...baselineTheme, ...(args.scheme === 'light' ? lightTheme : []))}>
      <div {...stylex.props(styles.controls)}>{[118, 159, 160, 240, 320, 400, 520].map(value => <Button key={value} size="sm" data-recovery-width={value} onPress={() => setWidth(value)}>{value}px</Button>)}{states.map(state => <Button key={state} size="sm" data-recovery-reset={state} onPress={() => reset(state)}>Start {state}</Button>)}</div>
      <div {...stylex.props(styles.controls)}>{variants.map((value, index) => <Button key={index} size="sm" data-recovery-variant={index} onPress={() => setVariant(index)}>{value.overview}{value.context}{value.spend}</Button>)}</div>
      <div {...stylex.props(styles.controls)}><Button size="sm" data-recovery-action="loading" onPress={() => setObservation(observationFor('loading'))}>Loading</Button><Button size="sm" data-recovery-action="failed" onPress={() => setObservation(current => ({ ...current, sync: failed }))}>Fail details</Button><Button size="sm" data-recovery-action="busy" onPress={recover}>Busy</Button></div>
      <div {...stylex.props(styles.controls)}>{(['forbidden', 'unsupported'] as const).flatMap(code => [false, true].map(retained => <Button key={`${code}-${retained}`} size="sm" data-recovery-failure-code={code} data-recovery-prior={String(retained)} onPress={() => setObservation({ content: retained ? 'busy' : 'loading', sync: { _tag: 'Failed', cause: { _tag: 'Server', code, message: code === 'forbidden' ? 'Access to this agent was denied.' : 'This gateway does not provide agent details.' } } })}>{code} · {retained ? 'retained' : 'first load'}</Button>))}</div>
      <section aria-label="Mounted Details recovery" data-recovery-stage style={{ width }} {...stylex.props(styles.stage)}><ThemePortal><Workbench {...args} key={generation} workspaceId={`${args.workspaceId}-${generation}`} focusedPaneKey={paneKey(details)} renderPane={pane => pane.view === 'details' ? <DetailsPane {...detailsStoryInputs(observation.content)} variants={variants[variant]!} state={observation.content} syncStatus={observation.sync} hasPriorContent={observation.content !== 'loading' && observation.content !== 'error' && observation.content !== 'new'} intent={intent} onIntentChange={setIntent} onOpen={() => { throw new Error('Recovery gate unexpectedly opened a resource') }} onRetry={recover} /> : args.renderPane?.(pane)} /></ThemePortal></section>
    </div>
  },
} satisfies Meta<Args>
export default meta
type Story = StoryObj<typeof meta>
type Play = { canvasElement: HTMLElement }

function requireContract(condition: unknown, reason: string): asserts condition { if (!condition) throw new Error(reason) }
async function rejectsControl(reason: string, assertion: () => void | Promise<void>) {
  let rejected = false
  try { await assertion() } catch (error) { requireContract(error instanceof Error && error.message === reason, `Unexpected control failure: ${String(error)}`); rejected = true }
  requireContract(rejected, `Negative control did not reject: ${reason}`)
}
async function frames(count = 3) { for (let index = 0; index < count; index++) await new Promise<void>(resolve => requestAnimationFrame(() => resolve())) }
const delay = (ms: number) => new Promise<void>(resolve => setTimeout(resolve, ms))
/** A pane wider than its window has no product host: header-clip sweeps mount only widths the viewport can show, so 520 runs in the 1440 rows. */
const hosted = (canvas: HTMLElement, candidates: readonly number[]) => candidates.filter(width => width <= canvas.ownerDocument.defaultView!.innerWidth)
function element<T extends HTMLElement = HTMLElement>(root: ParentNode, selector: string): T { const found = root.querySelector<T>(selector); requireContract(found !== null, `Missing recovery element: ${selector}`); return found }
const paneFor = (canvas: HTMLElement) => element(canvas, '[data-testid="details-pane"]')
const headFor = (pane: HTMLElement) => element(pane.closest('[data-testid="tab-group"]')!, '[data-pane-header]')
function click(canvas: HTMLElement, selector: string) { element(canvas, selector).click() }
async function start(canvas: HTMLElement, width: number, variant: DetailsVariants, state: DetailsState) {
  await canvas.ownerDocument.fonts.ready
  click(canvas, `[data-recovery-width="${width}"]`)
  click(canvas, `[data-recovery-variant="${variants.indexOf(variant)}"]`)
  const previous = canvas.querySelector('[data-testid="details-pane"]')
  click(canvas, `[data-recovery-reset="${state}"]`)
  await waitFor(() => { const pane = paneFor(canvas); requireContract(pane !== previous && pane.dataset.detailsPaneState === state && Math.abs(pane.getBoundingClientRect().width - width) <= 2, 'Host did not mount the requested recovery case') })
  await frames()
  paneFor(canvas).scrollTop = 0
  return paneFor(canvas)
}
async function action(canvas: HTMLElement, name: 'loading' | 'failed' | 'busy', settle = 100) {
  click(canvas, `[data-recovery-action="${name}"]`)
  await waitFor(() => expect(paneFor(canvas)).toHaveAttribute('data-details-sync-state', name === 'failed' ? 'Failed' : name === 'loading' ? 'Connecting' : 'Live'))
  await frames()
  await delay(settle)
}
const shown = (node: Element | null): node is HTMLElement => Boolean(node && node.getBoundingClientRect().width > 0 && getComputedStyle(node).visibility !== 'hidden' && getComputedStyle(node).display !== 'none')
function clipFor(head: HTMLElement) {
  const box = head.getBoundingClientRect()
  const view = head.ownerDocument.defaultView!
  const clip = { left: Math.max(0, box.left), right: Math.min(view.innerWidth, box.right), top: Math.max(0, box.top), bottom: Math.min(view.innerHeight, box.bottom) }
  for (let parent = head.parentElement; parent; parent = parent.parentElement) {
    const css = getComputedStyle(parent), rect = parent.getBoundingClientRect()
    if (/hidden|clip|auto|scroll/.test(css.overflowX)) { clip.left = Math.max(clip.left, rect.left + parent.clientLeft); clip.right = Math.min(clip.right, rect.left + parent.clientLeft + parent.clientWidth) }
    if (/hidden|clip|auto|scroll/.test(css.overflowY)) { clip.top = Math.max(clip.top, rect.top + parent.clientTop); clip.bottom = Math.min(clip.bottom, rect.top + parent.clientTop + parent.clientHeight) }
  }
  return clip
}
function assertHeader(pane: HTMLElement) {
  const head = headFor(pane), box = head.getBoundingClientRect(), clip = clipFor(head)
  const error = pane.querySelector('[data-details-error]')
  const first = error ? error.firstElementChild : pane.querySelector('[data-details-heading]')
  requireContract(Math.abs(box.height - 32) <= 0.5, 'Header is not a fixed 32px row')
  requireContract(Boolean(first) && Math.abs(first!.getBoundingClientRect().top - box.bottom - 8) <= 0.5, 'First content is not anchored at header bottom plus 8px')
  requireContract(![...pane.querySelectorAll('*'), ...head.querySelectorAll('*')].some(node => node.childElementCount === 0 && node.textContent?.trim() === 'Sections'), 'Unexpected Sections preamble')
  requireContract(head.querySelectorAll('[data-pane-sync-status]').length === 1 && pane.querySelectorAll('[data-testid="sync-line"],[data-pane-sync-status]').length === 0, 'Status is not exclusive to the header')
  for (const control of [...head.querySelectorAll('[role="tab"],button,a[href],[role="button"],input,select,textarea')].filter(shown)) {
    const rect = control.getBoundingClientRect()
    requireContract(rect.left >= clip.left - 0.5 && rect.right <= clip.right + 0.5 && rect.top >= clip.top - 0.5 && rect.bottom <= clip.bottom + 0.5, 'Header control escapes the effective clip')
  }
}
function assertPhase(pane: HTMLElement, phase: 'live' | 'loading' | 'failed') {
  const head = headFor(pane), status = element(head, '[data-pane-sync-status]')
  const word = [...status.querySelectorAll('[data-pane-sync-word],[data-pane-sync-word-trigger]')].find(shown)
  const glyph = status.querySelector('[data-pane-sync-glyph]')
  requireContract(status.dataset.paneSyncPhase === phase, 'Header has the wrong observation phase')
  if (phase === 'live') requireContract(!word && !glyph && [...status.childNodes].filter(node => node instanceof Element && !node.matches('[data-pane-sync-announcer],:has([data-pane-sync-announcer])') && shown(node)).every(node => !node.textContent?.trim()), 'Live header shows a status')
  if (phase === 'loading') requireContract(!word && shown(glyph) && getComputedStyle(glyph!.firstElementChild!).animationName === 'none' && [...status.childNodes].filter(node => node instanceof Element && !node.matches('[data-pane-sync-announcer],:has([data-pane-sync-announcer])') && shown(node)).every(node => !node.textContent?.trim()), 'Loading is not one still glyph without a word')
  if (phase === 'failed') {
    requireContract(Boolean(word), 'Failure lacks its recovery status')
    const label = word!.querySelector('[data-pane-sync-word-label]'), glyphOnly = Boolean(label && getComputedStyle(label).display === 'none')
    requireContract(head.getBoundingClientRect().width >= 160 || glyphOnly, 'Narrow failure lacks glyph recovery')
    requireContract(glyphOnly ? word!.getBoundingClientRect().width === 24 && word!.getAttribute('aria-label') === 'Load failed. Open details to retry.' : word!.textContent?.trim() === 'Load failed' && word!.scrollWidth <= word!.clientWidth + 0.5 && getComputedStyle(word!).textOverflow !== 'ellipsis', 'Failure status is truncated or lacks its labelled 24px fallback')
    const rect = word!.getBoundingClientRect(), clip = clipFor(head)
    requireContract(rect.left >= clip.left - 0.5 && rect.right <= clip.right + 0.5, 'Failure status escapes header clip')
    const dot = word!.querySelector('span')
    requireContract(Boolean(dot) && getComputedStyle(dot!).borderRadius !== '0px', 'Failed header lacks its status glyph')
    requireContract([...status.querySelectorAll('[data-pane-sync-details-trigger],[data-pane-sync-word-trigger]')].some(shown), 'Failed header lacks visible Details recovery')
    requireContract(!head.querySelector('[data-command-id="sync.retry"]'), 'Obsolete header Retry is mounted')
  }
}
type Rect = { top: number; left: number; width: number; height: number }
const rectOf = (node: Element): Rect => { const rect = node.getBoundingClientRect(); return { top: rect.top, left: rect.left, width: rect.width, height: rect.height } }
const equal = (a: unknown, b: unknown) => JSON.stringify(a) === JSON.stringify(b)
interface RecoveryGeometry { slot: Rect; header: Rect; sections: (Rect & { kind: string | null })[]; text: string }
function geometry(pane: HTMLElement): RecoveryGeometry {
  return { slot: rectOf(element(pane, '[data-details-content-slot]')), header: rectOf(headFor(pane)), sections: [...pane.querySelectorAll('section')].map(node => ({ kind: node.getAttribute('aria-label'), ...rectOf(node) })), text: element(pane, '[data-details-content-slot]').innerText }
}
interface Shift extends PerformanceEntry { value: number; hadRecentInput: boolean; sources?: readonly { node?: Node; previousRect: DOMRectReadOnly; currentRect: DOMRectReadOnly }[] }
function observeShifts() {
  requireContract(PerformanceObserver.supportedEntryTypes.includes('layout-shift'), 'LayoutShift observer unavailable')
  const entries: Shift[] = []
  const observer = new PerformanceObserver(list => entries.push(...list.getEntries() as Shift[]))
  observer.observe({ type: 'layout-shift', buffered: false })
  return { entries, stop: () => { entries.push(...observer.takeRecords() as Shift[]); observer.disconnect(); return entries } }
}
function assertShifts(entries: readonly Shift[]) {
  requireContract(entries.reduce((sum, entry) => sum + entry.value, 0) === 0 && entries.filter(entry => !entry.hadRecentInput).reduce((sum, entry) => sum + entry.value, 0) === 0, 'Transition has nonzero raw layout shift or CLS')
}
function record(canvas: HTMLElement, name: string, data: unknown) { canvas.setAttribute(`data-recovery-${name}`, JSON.stringify(data)) }

function assertPresentation(pane: HTMLElement) {
  const head = headFor(pane), state = pane.dataset.detailsPaneState, sync = pane.dataset.detailsSyncState
  requireContract(!/\bUnknown\b/.test(pane.innerText), 'Details renders Unknown instead of measured or pending evidence')
  requireContract(pane.querySelectorAll('[title]').length === 0, 'Details paints a native title')
  const sectionsControl = head.querySelector('[aria-label="Visible sections"]')
  requireContract(Boolean(sectionsControl) === (sync === 'Live' && state !== 'loading' && state !== 'error'), 'Visible sections is not limited to populated live content')
  if (sync === 'Failed') for (const ink of pane.querySelectorAll('[data-details-skeleton] *')) {
    const css = getComputedStyle(ink)
    if (css.visibility === 'hidden' || css.display === 'none') continue
    let opacity = 1
    for (let node: Element | null = ink; node; node = node.parentElement) opacity *= Number(getComputedStyle(node).opacity)
    requireContract(opacity === 0 || ink.getBoundingClientRect().width === 0 || ink.getBoundingClientRect().height === 0, 'Failure exposes hidden pending ink')
  }
  if (state === 'loading' && sync !== 'Failed') {
    requireContract(!/Observing/.test(pane.textContent ?? ''), 'Loading repeats per-section observation text')
    requireContract(pane.querySelectorAll('[data-sync-state="Connecting"]').length + head.querySelectorAll('[data-sync-state="Connecting"]').length === 1, 'Loading does not have exactly one connection status')
    for (const kind of ['overview', 'context', 'spend']) requireContract(Boolean(pane.querySelector(`[data-details-skeleton="${kind}"]`)), 'Loading lacks a populated-footprint skeleton')
    for (const shell of pane.querySelectorAll('[data-details-state] [data-details-skeleton]')) {
      requireContract(/rgba\(.*,\s*0\)|transparent/.test(getComputedStyle(shell).backgroundColor), 'Loading resource skeleton is a generic section block')
      requireContract([...shell.querySelectorAll('[data-details-resource-row]')].every(row => Boolean(row.closest('[data-details-skeleton-row]'))), 'Loading resource rows lack row-shaped skeletons')
      requireContract([...shell.querySelectorAll('[data-details-skeleton-row]')].some(row => [...row.querySelectorAll('span')].some(ink => getComputedStyle(ink).visibility === 'visible' && ink.getBoundingClientRect().width > 0)), 'Loading resource skeleton has no visible row ink')
    }
  }
  if (state === 'new') {
    const orientation = pane.querySelectorAll('[data-details-orientation]')
    requireContract(orientation.length === 1 && /context.*spend.*resources/i.test(orientation[0]!.textContent ?? ''), 'New agent lacks one calm Details orientation')
  }
  if (state === 'error' || (sync === 'Failed' && pane.dataset.detailsHasPriorContent === 'false')) {
    const error = element(pane, '[data-details-error]')
    requireContract(head.querySelector('[data-pane-sync-status][data-sync-state="Failed"]') !== null, 'Error lacks its header failure observation')
    const announcement = element(head, '[data-pane-sync-announcer][aria-live="polite"]').textContent ?? ''
    requireContract(announcement.includes("Couldn't load agent details") && announcement.includes('Retry to load details.'), 'Error recovery announcement is incomplete')
    requireContract(!/layout and view state are preserved/i.test(pane.innerText), 'Error makes an unsupported preservation claim')
    const reasons = error.querySelectorAll('[data-details-error-reason]')
    requireContract(reasons.length === 1 && reasons[0]!.textContent?.trim() === "Couldn't load agent details", 'Error lacks its one exact human failure heading')
    requireContract([...error.querySelectorAll('p')].every(text => getComputedStyle(text).textOverflow !== 'ellipsis' && getComputedStyle(text).whiteSpace !== 'nowrap'), 'Error explanation is ellipsized instead of wrapped')
    const retry = element<HTMLButtonElement>(error, '[data-command-id="sync.retry.body"]')
    requireContract(shown(retry) && !retry.disabled, 'Error lacks its enabled body recovery')
    const causes = error.querySelectorAll('[data-details-error-cause]')
    requireContract(causes.length === 1 && causes[0]!.textContent?.trim() === cause && shown(causes[0]!), 'Error omits or duplicates its exact host cause')
    requireContract(causes[0]!.getBoundingClientRect().height <= Number.parseFloat(getComputedStyle(causes[0]!).lineHeight) * 2 + 0.5, 'Error host cause exceeds two wrapped lines')
    for (const section of pane.querySelectorAll('section')) if (['Context', 'Spend'].includes(section.getAttribute('aria-label') ?? '')) requireContract(Boolean(section.closest('[inert]')) && getComputedStyle(section).visibility === 'hidden', 'Initial failure exposes pending chart content')
  }
}
async function anchorPlay({ canvasElement: canvas }: Play) {
  const measurements = []
  for (const width of hosted(canvas, widths)) for (const state of states) {
    const pane = await start(canvas, width, variants[defaultVariant]!, state)
    assertHeader(pane); assertPresentation(pane)
    measurements.push({ width, state, header: rectOf(headFor(pane)) })
  }
  const head = headFor(paneFor(canvas)), height = head.style.height, minHeight = head.style.minHeight
  head.style.height = '44px'; head.style.minHeight = '44px'
  try { await frames(); await rejectsControl('Header is not a fixed 32px row', () => assertHeader(paneFor(canvas))) } finally { head.style.height = height; head.style.minHeight = minHeight }
  await frames(); assertHeader(paneFor(canvas))
  const control = element(head, '[role="tab"]'), transform = control.style.transform
  control.style.transform = 'translateY(40px)'
  try { await rejectsControl('Header control escapes the effective clip', () => assertHeader(paneFor(canvas))) } finally { control.style.transform = transform }
  assertHeader(paneFor(canvas)); record(canvas, 'anchors', measurements)
  const pending = await start(canvas, 320, variants[defaultVariant]!, 'loading')
  const shell = element(pending, '[data-details-state] [data-details-skeleton]'), background = shell.style.backgroundColor
  shell.style.backgroundColor = 'rgb(80, 80, 80)'
  try { await rejectsControl('Loading resource skeleton is a generic section block', () => assertPresentation(pending)) } finally { shell.style.backgroundColor = background }
  assertPresentation(pending)
  const error = await start(canvas, 320, variants[defaultVariant]!, 'error'), reason = element(error, '[data-details-error-reason]'), text = reason.textContent
  reason.textContent = 'Unavailable'
  try { await rejectsControl('Error lacks its one exact human failure heading', () => assertPresentation(error)) } finally { reason.textContent = text }
  assertPresentation(error)
  // The painted error identity; the inert pending twin above it never reaches innerText.
  const identity = element(error, '[data-details-error] strong'), title = identity.textContent
  identity.textContent = 'Unknown'
  try { await rejectsControl('Details renders Unknown instead of measured or pending evidence', () => assertPresentation(error)) } finally { identity.textContent = title }
  assertPresentation(error)
  // The clamped cause discloses its full text through the kit tooltip, never a native title.
  const causeTrigger = element(error, '[data-details-error-cause]'), doc = canvas.ownerDocument
  causeTrigger.setAttribute('title', cause)
  try { await rejectsControl('Details paints a native title', () => assertPresentation(error)) } finally { causeTrigger.removeAttribute('title') }
  assertPresentation(error)
  causeTrigger.focus(); await userEvent.keyboard('{Tab}{Shift>}{Tab}{/Shift}')
  const causeTip = await within(doc.body).findByRole('tooltip')
  const assertCauseTip = () => requireContract(doc.activeElement === causeTrigger && causeTip.isConnected && causeTip.textContent === cause, 'Clamped host cause lacks its full keyboard tooltip')
  assertCauseTip()
  const tipText = causeTip.textContent
  try { causeTip.textContent = cause.slice(0, 12); await rejectsControl('Clamped host cause lacks its full keyboard tooltip', assertCauseTip) } finally { causeTip.textContent = tipText }
  assertCauseTip()
  causeTrigger.blur()
}

interface PendingGeometry {
  bodies: { kind: string | null; top: number; height: number; bodyHeight: number }[]
  sections: { kind: string | null; top: number; height: number }[]
  rows: { label: string | null; top: number; height: number }[]
}
function pendingGeometry(pane: HTMLElement): PendingGeometry {
  return {
    bodies: [...pane.querySelectorAll('[data-details-chart]')].map(body => ({ kind: body.getAttribute('data-details-chart'), top: body.closest('section')!.getBoundingClientRect().top, height: body.closest('section')!.getBoundingClientRect().height, bodyHeight: body.getBoundingClientRect().height })),
    sections: [...pane.querySelectorAll('[data-details-state] > section')].map(section => ({ kind: section.getAttribute('aria-label'), top: section.getBoundingClientRect().top, height: section.getBoundingClientRect().height })),
    rows: [...pane.querySelectorAll('[data-details-state] [data-details-resource-row]')].map(row => ({ label: [row.querySelector('[data-details-resource-title]')?.textContent, row.querySelector('[data-resource-meta-value]')?.textContent].filter(Boolean).join(' ') || null, top: row.getBoundingClientRect().top, height: row.getBoundingClientRect().height })),
  }
}
function assertPending(before: PendingGeometry, pane: HTMLElement) {
  const after = pendingGeometry(pane)
  requireContract(before.bodies.length === 3 && after.bodies.length === 3, 'Pending footprint lacks three chart bodies')
  for (const key of ['bodies', 'sections', 'rows'] as const) {
    requireContract(before[key].length === after[key].length, `Pending ${key} count changes`)
    before[key].forEach((value, index) => {
      const loaded = after[key][index]!
      requireContract(Object.entries(value).every(([field, expected]) => typeof expected === 'number' ? Math.abs(expected - Number((loaded as unknown as Record<string, unknown>)[field])) <= 0.1 : expected === (loaded as unknown as Record<string, unknown>)[field]), `Pending ${key} geometry or identity changes`)
    })
  }
}
async function pendingPlay({ canvasElement: canvas }: Play) {
  const measurements = []
  for (const width of widths) for (const variant of width < 320 ? representative : variants) {
    const pane = await start(canvas, width, variant, 'loading')
    assertPresentation(pane)
    requireContract(pane.querySelectorAll('[data-details-skeleton][data-details-chart]').length === 3, 'Pending footprint lacks three chart bodies')
    const before = pendingGeometry(pane), shifts = observeShifts()
    try { await action(canvas, 'busy'); requireContract(paneFor(canvas) === pane, 'Loading replaced the mounted pane'); assertPending(before, pane); assertShifts(shifts.stop()) } finally { shifts.stop() }
    measurements.push({ width, variant: variant.overview + variant.context + variant.spend, before, after: pendingGeometry(pane) })
  }
  const pane = paneFor(canvas), before = pendingGeometry(pane), body = element(pane, '[data-details-chart]'), padding = body.style.paddingBottom
  body.style.paddingBottom = '17px'
  try { await rejectsControl('Pending bodies geometry or identity changes', () => assertPending(before, pane)) } finally { body.style.paddingBottom = padding }
  assertPending(before, pane)
  const shifts = observeShifts(), section = element(pane, 'section'), margin = section.style.marginTop
  section.style.marginTop = '13px'
  try { await frames(6); await rejectsControl('Transition has nonzero raw layout shift or CLS', () => assertShifts(shifts.stop())) } finally { shifts.stop(); section.style.marginTop = margin }
  await frames(); record(canvas, 'pending', measurements)
}

function assertFailureGeometry(before: RecoveryGeometry, pane: HTMLElement) {
  const after = geometry(pane)
  requireContract(equal(before.slot, after.slot) && equal(before.header, after.header) && equal(before.sections, after.sections), 'Failure or recovery changes the reserved footprint')
  const first = element(pane, 'section').getBoundingClientRect()
  requireContract(Math.abs(first.top - headFor(pane).getBoundingClientRect().bottom) <= 0.5, 'Overview section does not start directly under header')
}
async function failurePlay({ canvasElement: canvas }: Play) {
  const measurements = []
  for (const width of hosted(canvas, widths)) for (const variant of representative) for (const initial of ['loading', 'busy'] as const) {
    const pane = await start(canvas, width, variant, initial), before = geometry(pane)
    for (const edge of ['failed', 'busy'] as const) {
      const shifts = observeShifts()
      try { await action(canvas, edge); assertShifts(shifts.stop()) } finally { shifts.stop() }
      requireContract(paneFor(canvas) === pane, 'Failure or recovery replaces the mounted pane')
      assertFailureGeometry(before, pane); assertHeader(pane); assertPresentation(pane); assertPhase(pane, edge === 'failed' ? 'failed' : 'live')
      const sectionsVisible = headFor(pane).querySelector('[aria-label="Visible sections"]') !== null
      requireContract(sectionsVisible === (edge === 'busy'), 'Visible sections is not limited to populated live content')
      if (edge === 'failed') {
        requireContract(pane.dataset.detailsHasPriorContent === String(initial === 'busy'), 'Failure has the wrong prior-content tag')
        if (initial === 'busy') requireContract(geometry(pane).text === before.text, 'Failure replaces retained content')
        else requireContract(equal(rectOf(element(pane, '[data-details-error]')), before.slot), 'First failure overlay does not occupy the pending slot')
      }
    }
    measurements.push({ width, variant: variant.overview + variant.context + variant.spend, initial, before, after: geometry(pane) })
  }
  const pane = paneFor(canvas), before = geometry(pane), slot = element(pane, '[data-details-content-slot]'), padding = slot.style.paddingBottom
  slot.style.paddingBottom = '19px'
  try { await rejectsControl('Failure or recovery changes the reserved footprint', () => assertFailureGeometry(before, pane)) } finally { slot.style.paddingBottom = padding }
  assertFailureGeometry(before, pane); record(canvas, 'failures', measurements)
}

function assertRecovery(doc: Document) {
  const count = [...doc.querySelectorAll<HTMLElement>('[data-command-id^="sync.retry"]')].filter(node => shown(node) && node.getBoundingClientRect().height > 0 && !node.closest('[inert],[aria-hidden="true"]') && !(node as HTMLButtonElement).disabled && node.tabIndex >= 0).length
  requireContract(count === 1, 'Recovery is not uniquely exposed')
}
function assertDialog(dialog: HTMLElement) {
  const doc = dialog.ownerDocument, box = dialog.getBoundingClientRect()
  const text = (ids: string | null) => (ids ?? '').split(/\s+/).filter(Boolean).map(id => doc.getElementById(id)?.textContent ?? '').join(' ')
  requireContract((text(dialog.getAttribute('aria-labelledby')) || dialog.getAttribute('aria-label')) === 'Load failed' && text(dialog.getAttribute('aria-describedby')).includes("Couldn't load agent details") && text(dialog.getAttribute('aria-describedby')).includes('Retry to load details.'), 'Recovery dialog lacks its announced name, cause and recovery')
  requireContract(dialog.contains(doc.activeElement) && !dialog.closest('[aria-hidden="true"],[inert]'), 'Recovery dialog is not focused and exposed')
  requireContract(dialog.textContent?.includes(cause) === true, 'Recovery dialog omits the full host cause')
  requireContract(box.width >= 200 && box.left >= 0 && box.right <= doc.defaultView!.innerWidth && box.bottom <= doc.defaultView!.innerHeight, 'Recovery dialog is not readable prose inside the viewport')
}
const detailsTrigger = (node: Element | null) => Boolean(node?.matches('[data-pane-sync-details-trigger],[data-pane-sync-word-trigger]'))
async function tabUntil(doc: Document, predicate: (node: Element | null) => boolean, count: number) {
  for (let index = 0; index < count && !predicate(doc.activeElement); index++) await userEvent.tab()
  requireContract(predicate(doc.activeElement), 'Keyboard did not reach the recovery control')
}
function assertRestored(pane: HTMLElement, tab: HTMLElement) { requireContract(pane.ownerDocument.activeElement === tab && tab.getAttribute('aria-selected') === 'true' && !pane.ownerDocument.querySelector('[data-pane-sync-details]'), 'Retry did not close Details and restore the stable selected tab') }
async function keyboardPlay({ canvasElement: canvas }: Play) {
  const measurements = [], doc = canvas.ownerDocument
  for (const width of [118, 159, 160, 240, 320, 400, 520]) for (const retained of [false, true]) {
    const pane = await start(canvas, width, variants[defaultVariant]!, retained ? 'busy' : 'error')
    if (retained) await action(canvas, 'failed')
    const tab = element(headFor(pane), '[role="tab"][aria-selected="true"]')
    requireContract(!doc.querySelector('[data-command-id="sync.retry"]'), 'Obsolete header Retry is mounted')
    if (!retained) assertRecovery(doc)
    tab.focus(); await userEvent.tab(); await tabUntil(doc, detailsTrigger, 8)
    const trigger = doc.activeElement as HTMLElement
    await userEvent.keyboard('{Enter}')
    const dialog = await within(doc.body).findByRole('dialog', { name: 'Load failed' })
    assertDialog(dialog); assertRecovery(doc)
    const description = dialog.getAttribute('aria-describedby')!
    dialog.removeAttribute('aria-describedby')
    try { await rejectsControl('Recovery dialog lacks its announced name, cause and recovery', () => assertDialog(dialog)) } finally { dialog.setAttribute('aria-describedby', description) }
    const bodyRetry = pane.querySelector<HTMLElement>('[data-command-id="sync.retry.body"]')
    if (bodyRetry) {
      const inertAncestor = bodyRetry.closest('[inert]')
      requireContract(!shown(bodyRetry) && inertAncestor !== null, 'Body recovery remains exposed while Details is open')
    }
    const duplicateRetry = element(dialog, '[data-command-id="sync.retry.details"]').cloneNode(true)
    dialog.append(duplicateRetry)
    try { await rejectsControl('Recovery is not uniquely exposed', () => assertRecovery(doc)) } finally { duplicateRetry.parentNode?.removeChild(duplicateRetry) }
    await tabUntil(doc, node => node?.getAttribute('data-command-id') === 'sync.retry.details', 4)
    await userEvent.keyboard('{Escape}')
    await waitFor(() => requireContract(!doc.querySelector('[data-pane-sync-details]') && doc.activeElement === trigger, 'Escape did not restore the Details trigger'))
    if (retained) { await userEvent.keyboard('{Enter}'); await within(doc.body).findByRole('dialog', { name: 'Load failed' }) }
    await tabUntil(doc, node => node?.getAttribute('data-command-id') === (retained ? 'sync.retry.details' : 'sync.retry.body'), 13)
    assertRecovery(doc); await userEvent.keyboard('{Enter}')
    await waitFor(() => { expect(pane).toHaveAttribute('data-details-sync-state', 'Live'); assertRestored(pane, tab) })
    const focusThief = element(canvas, '[data-recovery-action="busy"]'); focusThief.focus()
    try { await rejectsControl('Retry did not close Details and restore the stable selected tab', () => assertRestored(pane, tab)) } finally { tab.focus() }
    assertRestored(pane, tab); measurements.push({ width, retained, stableTab: tab.getAttribute('aria-label') })
  }
  record(canvas, 'keyboard', measurements)
}

type GlyphSample = { t: number; visible: boolean; word: boolean; animation: string | null }
/**
 * Samples are timed from the click; the first shares the Loading commit's task. `committed` is when the story observes that commit,
 * and `settled` is the second sample after a 300ms reference timer armed then fired.
 */
type GlyphTiming = { samples: readonly GlyphSample[]; committed: number; settled: number }
function assertTiming({ samples, settled }: GlyphTiming) {
  // The glyph's timer is armed in the commit's passive effects: after the click, and before the story observes the commit, because the
  // rest of that flush (the pending body's effects and forced layout) still runs first. So the earliest bounds are the click in wall time
  // and the commit's own task in event order (no timer runs inside it); the latest bound is the reference, armed after the glyph's timer.
  const index = samples.findIndex(sample => sample.visible), first = samples[index]?.t
  requireContract(index > 0 && first !== undefined && first >= 290 && first <= settled, 'Loading glyph does not first appear around 300ms')
  requireContract(!samples.some(sample => sample.word), 'Loading shows a status word')
  requireContract(!samples.some(sample => sample.animation !== null && sample.animation !== 'none'), 'Loading glyph animates')
  requireContract(!samples.some((sample, index) => index > 0 && samples[index - 1]!.visible && !sample.visible), 'Loading glyph flickers')
}
async function sampleGlyph(canvas: HTMLElement, mutate?: (glyph: HTMLElement) => void): Promise<GlyphTiming> {
  const head = headFor(paneFor(canvas)), samples: GlyphSample[] = []
  const clicked = performance.now()
  click(canvas, '[data-recovery-action="loading"]')
  // React commits the click's discrete update in a microtask, after rendering the whole pending Details body; the glyph's delay
  // starts in that commit. Timing the upper bound from before the click charged that body render to the glyph.
  await Promise.resolve()
  requireContract(element(head, '[data-pane-sync-status]').dataset.paneSyncPhase === 'loading', 'Loading did not commit with its click')
  const committed = performance.now() - clicked
  // The upper bound is event-loop order, not wall time: a starved main thread delays this reference and the glyph's own timer alike.
  const reference = { due: Infinity }
  setTimeout(() => { reference.due = performance.now() - clicked }, 300)
  const sample = () => {
    const glyph = head.querySelector<HTMLElement>('[data-pane-sync-glyph]'), t = performance.now() - clicked
    if (glyph && mutate) mutate(glyph)
    samples.push({ t, visible: shown(glyph), word: [...head.querySelectorAll('[data-pane-sync-word],[data-pane-sync-word-trigger]')].some(shown), animation: glyph ? getComputedStyle(glyph.firstElementChild!).animationName : null })
    return t
  }
  // The first sample shares the commit's task, so the glyph's timer cannot have run yet.
  sample()
  let afterDue = 0, settled = Infinity
  do {
    await frames(1)
    const t = sample()
    if (t >= reference.due && ++afterDue === 2) settled = t
  } while (performance.now() - clicked < committed + 700 || afterDue < 2)
  return { samples, committed, settled }
}
function assertAnnouncer(head: HTMLElement) { const regions = head.querySelectorAll('[aria-live]'); requireContract(regions.length === 1 && regions[0]!.getAttribute('aria-live') === 'polite', 'Header does not have exactly one polite live region') }
function assertSpoken(spoken: readonly string[], phase: string, expected: number) {
  requireContract(spoken.length === expected, 'Live region speaks the wrong number of times')
  if (phase === 'failed') requireContract(/Couldn't load agent details.*Retry to load details\./.test(spoken[0] ?? ''), 'Failure announcement lacks cause and recovery')
  if (phase === 'busy' && expected === 1) requireContract(spoken[0] === 'Agent details loaded.', 'Recovery announcement has the wrong text')
}
async function feedbackPlay({ canvasElement: canvas }: Play) {
  const measurements = []
  for (const width of hosted(canvas, [118, 160, 240, 520])) {
    const pane = await start(canvas, width, variants[defaultVariant]!, 'busy'), head = headFor(pane), initial = rectOf(head)
    const initialTabLeft = element(head, '[role="tab"]').getBoundingClientRect().left
    assertHeader(pane); assertPhase(pane, 'live'); assertAnnouncer(head)
    const region = element(head, '[aria-live]'), spoken: string[] = []
    const announcer = new MutationObserver(records => {
      for (const mutation of records) for (const node of mutation.addedNodes) { const text = node.textContent?.trim(); if (text) spoken.push(text) }
      if (records.some(mutation => mutation.type === 'characterData')) spoken.push(region.textContent!.trim())
    })
    announcer.observe(region, { childList: true, subtree: true, characterData: true })
    const shifts = observeShifts(), edges = []
    try {
      const timing = await sampleGlyph(canvas); assertTiming(timing); assertSpoken(spoken, 'loading', 0)
      assertHeader(pane); assertPhase(pane, 'loading')
      requireContract(equal(initial, rectOf(head)), 'Header box changes during observation transitions')
      requireContract(Math.abs(element(head, '[role="tab"]').getBoundingClientRect().left - initialTabLeft) <= 0.5, 'Header title moves during observation transitions')
      for (const [edge, expected] of [['failed', 1], ['busy', 1], ['failed', 1], ['loading', 0], ['busy', 1]] as const) {
        spoken.length = 0; await action(canvas, edge, 500)
        assertSpoken(spoken, edge, expected); assertHeader(pane); assertPhase(pane, edge === 'busy' ? 'live' : edge)
        requireContract(equal(initial, rectOf(head)), 'Header box changes during observation transitions')
        requireContract(Math.abs(element(head, '[role="tab"]').getBoundingClientRect().left - initialTabLeft) <= 0.5, 'Header title moves during observation transitions')
        edges.push({ edge, spoken: [...spoken] })
      }
      assertShifts(shifts.stop()); measurements.push({ width, firstVisible: timing.samples.find(sample => sample.visible)!.t, committed: timing.committed, settled: timing.settled, edges })
      const polite = region.getAttribute('aria-live')!; region.setAttribute('aria-live', 'assertive')
      try { await rejectsControl('Header does not have exactly one polite live region', () => assertAnnouncer(head)) } finally { region.setAttribute('aria-live', polite) }
      // Mutate the real region and let its observer report duplicate speech: no fabricated entry.
      spoken.length = 0; const duplicate = region.firstElementChild!.cloneNode(true); region.append(duplicate); await delay(0)
      try { await rejectsControl('Live region speaks the wrong number of times', () => assertSpoken(spoken, 'loading', 0)) } finally { duplicate.parentNode?.removeChild(duplicate) }
    } finally { announcer.disconnect(); shifts.stop() }
  }
  await start(canvas, 118, variants[defaultVariant]!, 'busy')
  const early = await sampleGlyph(canvas, glyph => { glyph.style.visibility = 'visible' })
  await rejectsControl('Loading glyph does not first appear around 300ms', () => assertTiming(early))
  const glyph = element(headFor(paneFor(canvas)), '[data-pane-sync-glyph]'); glyph.style.removeProperty('visibility')
  await start(canvas, 118, variants[defaultVariant]!, 'busy'); assertTiming(await sampleGlyph(canvas))
  record(canvas, 'feedback', measurements)
}

function exposed(node: Element) {
  for (let current: Element | null = node; current; current = current.parentElement) if (current.matches('[aria-hidden="true"],[inert]') || getComputedStyle(current).visibility === 'hidden' || getComputedStyle(current).display === 'none') return false
  return true
}
function identityInk(overview: HTMLElement) {
  const walker = overview.ownerDocument.createTreeWalker(overview, NodeFilter.SHOW_TEXT), inks: { text: string; parent: HTMLElement }[] = []
  for (let node = walker.nextNode(); node; node = walker.nextNode()) {
    const text = node.textContent?.trim(), parent = node.parentElement!, css = getComputedStyle(parent)
    if (text && !/^[·\s]+$/.test(text) && css.color !== 'rgba(0, 0, 0, 0)' && css.visibility !== 'hidden') inks.push({ text, parent })
  }
  return inks
}
function assertIdentity(pane: HTMLElement, title: string) {
  const overview = element(pane, '[data-details-skeleton="overview"]'), inks = identityInk(overview)
  requireContract(inks.length > 0 && element(overview, 'strong').textContent?.trim() === title, 'Loading does not preserve the known painted title')
  requireContract(inks.every(ink => exposed(ink.parent)), 'Painted known identity is absent from the accessibility tree')
  const fixture = makeDetailsFixture('busy').overview
  for (const known of [fixture.machine, fixture.model]) requireContract(Boolean(known) && inks.some(ink => ink.text.includes(known!)), 'Loading omits the already-known machine or model')
  const walker = overview.ownerDocument.createTreeWalker(overview, NodeFilter.SHOW_TEXT), accessible: string[] = []
  for (let node = walker.nextNode(); node; node = walker.nextNode()) if (exposed(node.parentElement!)) accessible.push(node.textContent ?? '')
  for (const placeholder of ['Since', 'Harness', 'Age']) requireContract(!new RegExp(`\\b${placeholder}\\b`).test(accessible.join(' ')) || inks.some(ink => ink.text.includes(placeholder)), 'Overview placeholder leaks into the accessibility tree')
}
async function identityPlay({ canvasElement: canvas }: Play) {
  const measurements = []
  for (const overview of ['O1', 'O2', 'O3'] as const) for (const width of [160, 320, 520]) {
    const variant = variants.find(value => value.overview === overview && value.context === 'C2' && value.spend === 'S1')!
    const loaded = await start(canvas, width, variant, 'busy'), title = element(loaded, '[data-details-chart] strong').textContent!.trim()
    const pane = await start(canvas, width, variant, 'loading'); assertIdentity(pane, title)
    const strong = element(pane, '[data-details-skeleton="overview"] strong'); strong.setAttribute('aria-hidden', 'true')
    try { await rejectsControl('Painted known identity is absent from the accessibility tree', () => assertIdentity(pane, title)) } finally { strong.removeAttribute('aria-hidden') }
    assertIdentity(pane, title); measurements.push({ width, overview, title, identity: identityInk(element(pane, '[data-details-skeleton="overview"]')).map(ink => ink.text) })
  }
  record(canvas, 'identity', measurements)
}

function assertNoRetry(surface: HTMLElement, reason: string) {
  requireContract(![...surface.querySelectorAll<HTMLButtonElement>('button')].some(button => button.textContent?.trim() === 'Retry'), reason)
}
async function nonRetryablePlay({ canvasElement: canvas }: Play) {
  // The control is cloned from the real retryable body, reproducing the rejected unconditional action.
  const initial = await start(canvas, 320, variants[defaultVariant]!, 'error')
  const retryTemplate = element<HTMLButtonElement>(initial, '[data-command-id="sync.retry.body"]').cloneNode(true) as HTMLButtonElement
  const measurements = []
  for (const width of [159, 320]) for (const code of ['forbidden', 'unsupported'] as const) for (const retained of [false, true]) {
    click(canvas, `[data-recovery-width="${width}"]`)
    click(canvas, `[data-recovery-failure-code="${code}"][data-recovery-prior="${retained}"]`)
    await waitFor(() => expect(paneFor(canvas)).toHaveAttribute('data-details-has-prior-content', String(retained)))
    await frames()
    const pane = paneFor(canvas), guidance = code === 'forbidden' ? 'Ask an administrator.' : 'These details are not available here.'
    assertNoRetry(pane, 'Non-retryable first-load failure offers Retry')
    if (!retained) {
      const body = element(pane, '[data-details-error]')
      requireContract(element(body, '[data-details-error-recovery]').textContent === guidance, 'Non-retryable body omits actionable recovery guidance')
      const badRetry = retryTemplate.cloneNode(true) as HTMLButtonElement
      body.appendChild(badRetry)
      try { await rejectsControl('Non-retryable first-load failure offers Retry', () => assertNoRetry(pane, 'Non-retryable first-load failure offers Retry')) } finally { badRetry.remove() }
      assertNoRetry(pane, 'Non-retryable first-load failure offers Retry')
    }
    const head = headFor(pane)
    const trigger = [...head.querySelectorAll<HTMLButtonElement>('[data-pane-sync-word-trigger],[data-pane-sync-details-trigger]')].find(shown)!
    requireContract(trigger !== undefined && !/\bretry\b/i.test(trigger.getAttribute('aria-label') ?? ''), 'Non-retryable header promises Retry')
    await userEvent.click(trigger)
    const dialog = await within(canvas.ownerDocument.body).findByRole('dialog', { name: 'Load failed' })
    requireContract(dialog.textContent?.includes(guidance) === true, 'Non-retryable popover omits actionable recovery guidance')
    assertNoRetry(dialog, 'Non-retryable popover offers Retry')
    const badRetry = retryTemplate.cloneNode(true) as HTMLButtonElement
    dialog.appendChild(badRetry)
    try { await rejectsControl('Non-retryable popover offers Retry', () => assertNoRetry(dialog, 'Non-retryable popover offers Retry')) } finally { badRetry.remove() }
    assertNoRetry(dialog, 'Non-retryable popover offers Retry')
    await userEvent.keyboard('{Escape}')
    await waitFor(() => expect(dialog).not.toBeInTheDocument())
    measurements.push({ width, code, retained, guidance, bodyRetry: false, popoverRetry: false })
  }
  record(canvas, 'non-retryable', measurements)
}

export const AllStates: Story = { play: anchorPlay }
export const PendingFootprints: Story = { args: { initialState: 'loading' }, play: pendingPlay }
export const FailureTransitions: Story = { play: failurePlay }
export const KeyboardRecovery: Story = { args: { initialState: 'error' }, play: keyboardPlay }
export const HeaderFeedback: Story = { play: feedbackPlay }
export const KnownLoadingIdentity: Story = { args: { initialState: 'loading' }, play: identityPlay }
export const NonRetryableFailures: Story = { args: { initialState: 'error' }, play: nonRetryablePlay }
