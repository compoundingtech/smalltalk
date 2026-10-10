import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { flushSync } from 'react-dom'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { expect, userEvent, waitFor, within } from 'storybook/test'
import { Button } from 'react-aria-components'
import { EmbraceScrollViewport, type EmbraceScrollViewportHandle } from './assistant-ui/EmbraceScrollViewport'
import { baselineTheme } from './assistant-ui/neutral-theme'
import { lightTheme, type Scheme } from './assistant-ui/composition-theme'
import { surfaceVars as surface, textVars as ink, borderVars as border, spaceVars as s, typeVars as t, geometryVars as g } from './assistant-ui/composition-tokens.stylex'

/** `unmanaged` is the control lane: the same rows in a plain scroller without the kit viewport. */
type Lane = 'kit' | 'unmanaged'
interface Drivers {
  readonly stream: () => void
  readonly composer: (lines: number) => void
  readonly expandAbove: (open: boolean) => void
  /** Older history lands above every row; `preserve` routes the commit through the viewport's layout operation. */
  readonly prepend: (count: number, preserve: boolean) => void
  /** The host acknowledges the pending row under a new key without changing its content. */
  readonly acknowledge: () => void
}
let drivers: Drivers | undefined
const drive = () => { if (drivers === undefined) throw new Error('Follow story not mounted'); return drivers }

function FollowStory({ scheme = 'dark', lane = 'kit' }: { scheme?: Scheme; lane?: Lane }) {
  const viewport = React.useRef<EmbraceScrollViewportHandle>(null)
  const [chunks, setChunks] = React.useState(0)
  const [composerLines, setComposerLines] = React.useState(2)
  const [expanded, setExpanded] = React.useState(false)
  const [older, setOlder] = React.useState(0)
  const [latestKey, setLatestKey] = React.useState('latest')
  const rows = React.useMemo(() => [
    ...Array.from({ length: older }, (_, index) => ({ id: `older-${older - index}`, text: `Earlier observation ${older - index}: backfilled history lands above the reader.` })),
    ...Array.from({ length: 40 }, (_, index) => ({ id: `history-${index}`, text: `Message ${index + 1}: the lane follows the live conversation until the reader chooses to read earlier messages.` })),
    { id: latestKey, text: `Latest reply.${'\nA streamed line extends the reply.'.repeat(chunks)}` },
  ], [chunks, older, latestKey])
  React.useLayoutEffect(() => {
    drivers = {
      stream: () => flushSync(() => setChunks(value => value + 1)),
      composer: lines => flushSync(() => setComposerLines(lines)),
      expandAbove: open => flushSync(() => setExpanded(open)),
      prepend: (count, preserve) => {
        const change = () => flushSync(() => setOlder(value => value + count))
        if (preserve && viewport.current !== null) viewport.current.preserveLayout(change)
        else change()
      },
      acknowledge: () => flushSync(() => setLatestKey('timeline-entry/ack')),
    }
    return () => { drivers = undefined }
  }, [])
  const content = rows.map(row => <article key={row.id} data-item-id={row.id} {...stylex.props(styles.row)}>
    <p {...stylex.props(styles.text)}>{row.text}</p>
    {row.id === 'history-3' && expanded && <pre data-testid="expanded-above" {...stylex.props(styles.expanded)}>{'An expanded observation above the reading line.\n'.repeat(10)}</pre>}
    {row.id === latestKey && <Button>Reply action</Button>}
  </article>)
  return <main {...stylex.props(styles.root, ...baselineTheme, scheme === 'light' && lightTheme)}>
    {lane === 'kit'
      ? <EmbraceScrollViewport ref={viewport} items={rows} data-testid="follow-lane" aria-label="Conversation history" tabIndex={0} {...stylex.props(styles.lane)} contentProps={stylex.props(styles.rows)}>{content}</EmbraceScrollViewport>
      : <div data-testid="follow-lane" aria-label="Conversation history" role="region" tabIndex={0} {...stylex.props(styles.lane, styles.unmanaged)}><div {...stylex.props(styles.rows)}>{content}</div></div>}
    <form data-follow-composer aria-label="Composer" {...stylex.props(styles.composer)}>
      <textarea aria-label="Message" rows={composerLines} readOnly value="Draft reply" {...stylex.props(styles.draft)} />
    </form>
  </main>
}

const meta = { title: 'Fractal UI/Transcript follow', component: FollowStory, args: { scheme: 'dark', lane: 'kit' }, parameters: { layout: 'fullscreen' }, argTypes: { scheme: { options: ['dark', 'light'], control: 'radio' }, lane: { options: ['kit', 'unmanaged'], control: 'radio' } } } satisfies Meta<typeof FollowStory>
export default meta
type Story = StoryObj<typeof meta>

const pillName = 'Scroll to end'
const gap = (lane: HTMLElement) => lane.scrollHeight - lane.clientHeight - lane.scrollTop
const settle = () => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve())))
/** Commit inside an animation frame, the way frame-batched runtimes publish, then read what that frame painted. */
const paintedAfter = <T,>(change: () => void, read: () => T) => new Promise<T>(resolve => requestAnimationFrame(() => {
  change()
  const channel = new MessageChannel()
  channel.port1.onmessage = () => resolve(read())
  channel.port2.postMessage(null)
}))
/** Control fault: resize delivery reaches the viewport one frame late, as a frame-scheduled repin would. */
const deferResize = () => {
  const Native = window.ResizeObserver
  window.ResizeObserver = class extends Native {
    constructor(callback: ResizeObserverCallback) { super((entries, observer) => { requestAnimationFrame(() => callback(entries, observer)) }) }
  }
  return () => { window.ResizeObserver = Native }
}
/** Counts animation frames the viewport schedules and the ones that actually run. */
function trackViewportFrames() {
  const native = window.requestAnimationFrame
  const counts = { requested: 0, ran: 0 }
  window.requestAnimationFrame = callback => {
    if (new Error().stack?.includes('EmbraceScrollViewport') !== true) return native.call(window, callback)
    counts.requested++
    return native.call(window, time => { counts.ran++; callback(time) })
  }
  return { counts, stop: () => { window.requestAnimationFrame = native } }
}

async function ready(canvasElement: HTMLElement) {
  await document.fonts.ready
  const canvas = within(canvasElement)
  const lane = await waitFor(() => canvas.getByTestId('follow-lane'))
  if (lane.dataset['followState'] === undefined) lane.scrollTop = lane.scrollHeight
  await waitFor(() => expect(gap(lane)).toBeLessThanOrEqual(1))
  await settle()
  return { canvas, lane }
}
async function expectAttached(canvasElement: HTMLElement, lane: HTMLElement) {
  await expect(lane.dataset['followState']).toBe('attached')
  await expect(gap(lane)).toBeLessThanOrEqual(1)
  await expect(within(canvasElement).queryByRole('button', { name: pillName })).toBeNull()
}
async function readerScrollsUp(lane: HTMLElement, by: number) {
  lane.dispatchEvent(new WheelEvent('wheel', { deltaY: -by, bubbles: true }))
  lane.scrollTop -= by
  await settle()
}
/** The first row starting inside the lane is the reader's line. */
function readingLine(lane: HTMLElement) {
  const top = lane.getBoundingClientRect().top
  const line = Array.from(lane.querySelectorAll<HTMLElement>('[data-item-id]')).find(row => row.getBoundingClientRect().top >= top)!
  return { line, offset: () => line.getBoundingClientRect().top - top }
}

/** Every streamed frame paints the lane at its end, whether the chunk lands inside a frame or between frames. */
async function provePinnedStreaming(canvasElement: HTMLElement) {
  const { lane } = await ready(canvasElement)
  const height = lane.scrollHeight
  const painted: number[] = []
  for (let chunk = 0; chunk < 40; chunk++) {
    if (chunk % 2 === 0) painted.push(await paintedAfter(() => drive().stream(), () => gap(lane)))
    else {
      drive().stream()
      // A chunk committed between frames: the frame that follows paints it.
      painted.push(await paintedAfter(() => undefined, () => gap(lane)))
    }
  }
  await expect(lane.scrollHeight - height, 'stream must grow the lane').toBeGreaterThan(400)
  await expect(Math.max(...painted), `painted distance from the end per frame: ${painted.join(',')}`).toBeLessThanOrEqual(1)
  await expectAttached(canvasElement, lane)
}
export const PinnedStreaming: Story = { play: async ({ canvasElement }) => { await provePinnedStreaming(canvasElement) } }
export const PinnedStreamingLight: Story = { ...PinnedStreaming, args: { scheme: 'light' } }
/** Control: frame-late resize repins paint at least one streamed frame away from the end. */
export const PinnedStreamingLateResizeControl: Story = { beforeEach: deferResize, play: async ({ canvasElement }) => {
  await expect(provePinnedStreaming(canvasElement)).rejects.toThrow(/painted distance/)
} }

/** Composer adoption resizes the lane; the frame that resizes it paints the end. */
async function proveComposerResize(canvasElement: HTMLElement) {
  const { lane } = await ready(canvasElement)
  for (const lines of [8, 3, 12, 2]) {
    const before = lane.clientHeight
    const painted = await paintedAfter(() => drive().composer(lines), () => ({ gap: gap(lane), height: lane.clientHeight }))
    await expect(Math.abs(painted.height - before), `composer at ${lines} lines must resize the lane`).toBeGreaterThan(20)
    await expect(painted.gap, `painted distance from the end after the composer took ${lines} lines`).toBeLessThanOrEqual(1)
  }
  await expectAttached(canvasElement, lane)
}
export const ComposerResizeRepins: Story = { play: async ({ canvasElement }) => { await proveComposerResize(canvasElement) } }
export const ComposerResizeRepinsLight: Story = { ...ComposerResizeRepins, args: { scheme: 'light' } }
export const ComposerResizeLateResizeControl: Story = { beforeEach: deferResize, play: async ({ canvasElement }) => {
  await expect(proveComposerResize(canvasElement)).rejects.toThrow(/painted distance/)
} }

/** Resize delivery that already pinned the lane consumes the follow frame a commit scheduled. */
async function proveNoRedundantFrame(canvasElement: HTMLElement) {
  const { lane } = await ready(canvasElement)
  const frames = trackViewportFrames()
  try {
    for (let chunk = 0; chunk < 12; chunk++) await paintedAfter(() => drive().stream(), () => gap(lane))
    await settle()
  } finally { frames.stop() }
  await expect(frames.counts.requested, 'commits must schedule a follow frame').toBeGreaterThanOrEqual(12)
  await expect(frames.counts.ran, 'follow frames ran after resize delivery already pinned the lane').toBe(0)
  await expectAttached(canvasElement, lane)
}
export const NoRedundantFollowFrame: Story = { play: async ({ canvasElement }) => { await proveNoRedundantFrame(canvasElement) } }
export const NoRedundantFollowFrameLight: Story = { ...NoRedundantFollowFrame, args: { scheme: 'light' } }
export const NoRedundantFollowFrameLateResizeControl: Story = { beforeEach: deferResize, play: async ({ canvasElement }) => {
  await expect(proveNoRedundantFrame(canvasElement)).rejects.toThrow(/follow frames ran/)
} }

/** Momentum keeps the reader's gesture until scrollend; momentum that reaches the end resumes following. */
async function proveMomentum(canvasElement: HTMLElement, landShortBy: number) {
  const { canvas, lane } = await ready(canvasElement)
  await readerScrollsUp(lane, 600)
  await expect(lane.dataset['followState']).toBe('detached')
  await waitFor(() => expect(canvas.getByRole('button', { name: pillName })).toBeVisible())
  // Momentum continues past the input window without new input; the reader still owns the lane.
  await new Promise(resolve => setTimeout(resolve, 400))
  lane.scrollTop -= 120
  await settle()
  await expect(lane.dataset['followState'], 'momentum after the input window must stay detached').toBe('detached')
  await new Promise(resolve => setTimeout(resolve, 400))
  lane.scrollTop = lane.scrollHeight - lane.clientHeight - landShortBy
  await settle()
  lane.dispatchEvent(new Event('scrollend'))
  await settle()
  await expectAttached(canvasElement, lane)
  const painted: number[] = []
  for (let chunk = 0; chunk < 6; chunk++) painted.push(await paintedAfter(() => drive().stream(), () => gap(lane)))
  await expect(Math.max(...painted), `painted distance after re-follow: ${painted.join(',')}`).toBeLessThanOrEqual(1)
}
export const MomentumReachesEndRefollows: Story = { play: async ({ canvasElement }) => { await proveMomentum(canvasElement, 0) } }
export const MomentumReachesEndRefollowsLight: Story = { ...MomentumReachesEndRefollows, args: { scheme: 'light' } }
/** Control: momentum stopping short of the end stays detached, so the attachment assertion rejects it. */
export const MomentumStopsShortControl: Story = { play: async ({ canvasElement }) => {
  await expect(proveMomentum(canvasElement, 60)).rejects.toThrow()
} }

/** A detached reader's line holds through growth below, growth and shrink above, and older history. */
async function proveDetachedHolds(canvasElement: HTMLElement) {
  const { canvas, lane } = await ready(canvasElement)
  await readerScrollsUp(lane, Math.round((lane.scrollHeight - lane.clientHeight) / 2))
  lane.dispatchEvent(new Event('scrollend'))
  await settle()
  const { line, offset } = readingLine(lane)
  const start = offset()
  for (const [label, change] of [
    ['growth below', () => { for (let chunk = 0; chunk < 3; chunk++) drive().stream() }],
    ['growth above', () => drive().expandAbove(true)],
    ['shrink above', () => drive().expandAbove(false)],
    ['older history above', () => drive().prepend(8, false)],
  ] as const) {
    change()
    await settle()
    await expect(Math.abs(offset() - start), `${label} moved the reading line`).toBeLessThanOrEqual(1)
  }
  await expect(lane.dataset['followState']).toBe('detached')
  await expect(canvas.getByRole('button', { name: pillName })).toBeVisible()
}
export const DetachedHolds: Story = { play: async ({ canvasElement }) => { await proveDetachedHolds(canvasElement) } }
export const DetachedHoldsLight: Story = { ...DetachedHolds, args: { scheme: 'light' } }
export const DetachedHoldsUnmanagedControl: Story = { args: { lane: 'unmanaged' }, play: async ({ canvasElement }) => {
  await expect(proveDetachedHolds(canvasElement)).rejects.toThrow(/growth above moved the reading line/)
} }

/** Backfill through `preserveLayout`: a following lane paints its end and a reader paints its line in the same frame. `growth` also streams the reply, which needs on-time resize delivery. */
async function proveBackfill(canvasElement: HTMLElement, preserve: boolean, growth = true) {
  const { lane } = await ready(canvasElement)
  const following = await paintedAfter(() => drive().prepend(8, preserve), () => gap(lane))
  await expect(following, 'painted distance from the end after backfill').toBeLessThanOrEqual(1)
  const painted: number[] = []
  if (growth) for (let chunk = 0; chunk < 6; chunk++) painted.push(await paintedAfter(() => drive().stream(), () => gap(lane)))
  await expect(Math.max(0, ...painted), `painted distance as the reply grows: ${painted.join(',')}`).toBeLessThanOrEqual(1)
  await readerScrollsUp(lane, Math.round((lane.scrollHeight - lane.clientHeight) / 2))
  lane.dispatchEvent(new Event('scrollend'))
  await settle()
  const { offset } = readingLine(lane)
  const start = offset()
  const moved = await paintedAfter(() => drive().prepend(8, preserve), () => Math.abs(offset() - start))
  await expect(moved, 'backfill painted the reading line elsewhere').toBeLessThanOrEqual(1)
  if (growth) for (let chunk = 0; chunk < 3; chunk++) drive().stream()
  await settle()
  await expect(Math.abs(offset() - start), 'reply growth moved the detached reader').toBeLessThanOrEqual(1)
  await expect(lane.dataset['followState']).toBe('detached')
}
export const BackfillKeepsMode: Story = { play: async ({ canvasElement }) => { await proveBackfill(canvasElement, true) } }
export const BackfillKeepsModeLight: Story = { ...BackfillKeepsMode, args: { scheme: 'light' } }
/** `preserveLayout` compensates inside the commit, so it holds even when resize delivery arrives a frame late. */
export const BackfillKeepsModeLateResize: Story = { beforeEach: deferResize, play: async ({ canvasElement }) => { await proveBackfill(canvasElement, true, false) } }
/** Control: with resize delivery late, backfill committed outside `preserveLayout` paints one frame displaced. */
export const BackfillOutsideViewportControl: Story = { beforeEach: deferResize, play: async ({ canvasElement }) => {
  await expect(proveBackfill(canvasElement, false, false)).rejects.toThrow(/painted distance from the end after backfill|backfill painted the reading line elsewhere/)
} }

/** An acknowledgement re-keys the pending row; the layout scroll it causes never detaches a following lane. */
async function proveAckRekey(canvasElement: HTMLElement, readerInput: boolean) {
  const { lane } = await ready(canvasElement)
  const previous = lane.querySelector<HTMLElement>('[data-item-id="latest"]')!
  const text = previous.textContent
  const height = lane.scrollHeight
  drive().acknowledge()
  await settle()
  await expect(previous.isConnected).toBe(false)
  await expect(lane.querySelector('[data-item-id="timeline-entry/ack"]')?.textContent).toBe(text)
  await expect(lane.scrollHeight).toBe(height)
  if (readerInput) lane.dispatchEvent(new WheelEvent('wheel', { deltaY: -47, bubbles: true }))
  lane.scrollTop -= 47
  await settle()
  await expectAttached(canvasElement, lane)
}
export const AckRekeyKeepsFollowing: Story = { play: async ({ canvasElement }) => { await proveAckRekey(canvasElement, false) } }
export const AckRekeyKeepsFollowingLight: Story = { ...AckRekeyKeepsFollowing, args: { scheme: 'light' } }
/** Control: the same 47px movement with reader input detaches, so the attachment assertion rejects it. */
export const AckRekeyReaderInputControl: Story = { play: async ({ canvasElement }) => {
  await expect(proveAckRekey(canvasElement, true)).rejects.toThrow()
} }

/** The floating pill appears only away from the end, sits above the composer and returns to the live edge. */
export const PillReattaches: Story = { play: async ({ canvasElement }) => {
  const { canvas, lane } = await ready(canvasElement)
  await expect(canvas.queryByRole('button', { name: pillName })).toBeNull()
  await readerScrollsUp(lane, 400)
  const pill = await waitFor(() => canvas.getByRole('button', { name: pillName }))
  await expect(pill).toBeVisible()
  // Control: the attachment assertion rejects a detached lane.
  await expect(expectAttached(canvasElement, lane)).rejects.toThrow()
  const composer = canvasElement.querySelector('[data-follow-composer]')!.getBoundingClientRect()
  const placed = pill.getBoundingClientRect()
  await expect(placed.bottom, 'pill must sit above the composer').toBeLessThanOrEqual(composer.top)
  await expect(placed.top, 'pill must sit inside the lane').toBeGreaterThanOrEqual(lane.getBoundingClientRect().top)
  const box = lane.getBoundingClientRect()
  await expect(Math.abs(placed.left + placed.width / 2 - (box.left + box.width / 2)), 'pill must be centred on the lane').toBeLessThanOrEqual(1)
  await userEvent.click(pill)
  await settle()
  await expectAttached(canvasElement, lane)
} }
export const PillReattachesLight: Story = { ...PillReattaches, args: { scheme: 'light' } }
/** Visibility has its own 40px band; it must not borrow the 1px end/rounding tolerance. */
async function provePillBand(canvasElement: HTMLElement, revealInsideBand = false) {
  const { canvas, lane } = await ready(canvasElement)
  await readerScrollsUp(lane, 20)
  await expect(lane.dataset['followState']).toBe('detached')
  if (revealInsideBand) canvasElement.querySelector<HTMLButtonElement>('[aria-label="Scroll to end"]')!.hidden = false
  await expect(canvas.queryByRole('button', { name: pillName }), '20px is inside the pill visibility band').toBeNull()
  await readerScrollsUp(lane, 20)
  await expect(canvas.queryByRole('button', { name: pillName }), '40px is inside the pill visibility band').toBeNull()
  await readerScrollsUp(lane, 1)
  await expect(canvas.getByRole('button', { name: pillName })).toBeVisible()
  await readerScrollsUp(lane, 359)
  await expect(Math.round(gap(lane))).toBe(400)
  await expect(canvas.getByRole('button', { name: pillName })).toBeVisible()
}
export const PillVisibilityBand: Story = { play: async ({ canvasElement }) => { await provePillBand(canvasElement) } }
export const PillVisibilityBandLight: Story = { ...PillVisibilityBand, args: { scheme: 'light' } }
export const PillInsideBandControl: Story = { play: async ({ canvasElement }) => {
  await expect(provePillBand(canvasElement, true)).rejects.toThrow(/20px is inside/)
} }
export const PillInsideBandControlLight: Story = { ...PillInsideBandControl, args: { scheme: 'light' } }

/** Tabbing to the lane or a row action is focus, not a request to read history. */
async function proveFocusKeepsFollowing(canvasElement: HTMLElement, navigation = false) {
  const { canvas, lane } = await ready(canvasElement)
  if (document.activeElement instanceof HTMLElement) document.activeElement.blur()
  await userEvent.tab()
  await expect(document.activeElement).toBe(lane)
  if (navigation) lane.dispatchEvent(new KeyboardEvent('keydown', { key: 'PageUp', bubbles: true }))
  for (let chunk = 0; chunk < 4; chunk++) {
    const painted = await paintedAfter(() => drive().stream(), () => gap(lane))
    await expect(painted, 'focus alone must keep streaming at the end').toBeLessThanOrEqual(1)
  }
  await expectAttached(canvasElement, lane)
  await userEvent.tab()
  await expect(document.activeElement).toBe(canvas.getByRole('button', { name: 'Reply action' }))
  for (let chunk = 0; chunk < 4; chunk++) {
    const painted = await paintedAfter(() => drive().stream(), () => gap(lane))
    await expect(painted, 'row-action focus alone must keep streaming at the end').toBeLessThanOrEqual(1)
  }
  await expectAttached(canvasElement, lane)
}
export const TabKeepsFollowing: Story = { play: async ({ canvasElement }) => { await proveFocusKeepsFollowing(canvasElement) } }
export const TabKeepsFollowingLight: Story = { ...TabKeepsFollowing, args: { scheme: 'light' } }
export const NavigationIntentControl: Story = { play: async ({ canvasElement }) => {
  await expect(proveFocusKeepsFollowing(canvasElement, true)).rejects.toThrow(/focus alone must keep/)
} }
export const NavigationIntentControlLight: Story = { ...NavigationIntentControl, args: { scheme: 'light' } }

/** Shift+Tab reaches the detached pill; Enter returns to the end and hands focus back to the draft. */
async function proveKeyboardPill(canvasElement: HTMLElement, activate = true) {
  const { canvas, lane } = await ready(canvasElement)
  await readerScrollsUp(lane, 400)
  const pill = canvas.getByRole('button', { name: pillName })
  const draft = canvas.getByRole('textbox', { name: 'Message' })
  draft.focus({ preventScroll: true })
  await userEvent.tab({ shift: true })
  await expect(document.activeElement).toBe(pill)
  if (activate) await userEvent.keyboard('{Enter}')
  await settle()
  await expectAttached(canvasElement, lane)
  await expect(document.activeElement).toBe(draft)
}
export const KeyboardPillReattaches: Story = { play: async ({ canvasElement }) => { await proveKeyboardPill(canvasElement) } }
export const KeyboardPillReattachesLight: Story = { ...KeyboardPillReattaches, args: { scheme: 'light' } }
export const KeyboardPillNotActivatedControl: Story = { play: async ({ canvasElement }) => {
  await expect(proveKeyboardPill(canvasElement, false)).rejects.toThrow()
} }
export const KeyboardPillNotActivatedControlLight: Story = { ...KeyboardPillNotActivatedControl, args: { scheme: 'light' } }

export const AllStates: Story = { render: args => <FollowStory {...args} /> }

const styles = stylex.create({
  root: { height: '100vh', width: '100%', boxSizing: 'border-box', display: 'flex', flexDirection: 'column', backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans },
  lane: { flex: '1 1 0', minHeight: 0, overflowY: 'auto' },
  unmanaged: { overflowAnchor: 'none' },
  rows: { display: 'flex', flexDirection: 'column', gap: s.md, paddingBlock: s.lg, paddingInline: s.lg },
  row: { borderBottomWidth: g.hairline, borderBottomStyle: 'solid', borderBottomColor: border.border },
  text: { margin: 0, whiteSpace: 'pre-wrap', fontSize: t.bodySize, lineHeight: t.bodyLeading },
  expanded: { margin: 0, fontSize: t.metaSize, lineHeight: t.metaLeading, color: ink.fgMuted },
  composer: { flexShrink: 0, display: 'flex', paddingBlock: s.md, paddingInline: s.lg, borderTopWidth: g.hairline, borderTopStyle: 'solid', borderTopColor: border.border },
  draft: { flex: '1 1 auto', resize: 'none', fontFamily: t.fontSans, fontSize: t.bodySize, lineHeight: t.bodyLeading, color: ink.fg, backgroundColor: surface.controlFill, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong },
})
