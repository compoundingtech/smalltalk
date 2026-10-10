import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { flushSync } from 'react-dom'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { expect, userEvent, waitFor, within } from 'storybook/test'
import { EmbraceVirtualConversation } from './assistant-ui/EmbraceVirtualConversation'
import { baselineTheme } from './assistant-ui/neutral-theme'
import { lightTheme, type Scheme } from './assistant-ui/composition-theme'
import { darkTheme as embraceDarkTheme } from './assistant-ui/embrace-theme'
import { surfaceVars as surface, textVars as ink, spaceVars as s, typeVars as t, geometryVars as g } from './assistant-ui/composition-tokens.stylex'

interface Row { readonly id: string; readonly chunks: number }
let stream: (() => void) | undefined
let resizeComposer: ((lines: number) => void) | undefined
const expanders = new Map<string, (open: boolean) => void>()
function VirtualRow({ row }: { readonly row: Row }) {
  const [open, setOpen] = React.useState(false)
  React.useLayoutEffect(() => {
    expanders.set(row.id, value => flushSync(() => setOpen(value)))
    return () => { expanders.delete(row.id) }
  }, [row.id])
  return <article {...stylex.props(styles.row)}><p {...stylex.props(styles.text)}>Message {row.id}: measured conversation history preserves the reader's line.{row.chunks > 0 ? '\nA streamed line extends the live reply.'.repeat(row.chunks) : ''}</p>{open && <pre {...stylex.props(styles.text)}>{'An expanded observation above the reader.\n'.repeat(8)}</pre>}</article>
}
const renderRow = (row: Row) => <VirtualRow row={row} />
function VirtualStory({ scheme = 'dark' }: { readonly scheme?: Scheme }) {
  const [chunks, setChunks] = React.useState(0)
  const [lines, setLines] = React.useState(2)
  const rows = React.useMemo(() => Array.from({ length: 60 }, (_, index) => ({ id: `entry-${index}`, chunks: index === 59 ? chunks : 0 })), [chunks])
  React.useLayoutEffect(() => {
    stream = () => flushSync(() => setChunks(value => value + 1))
    resizeComposer = value => flushSync(() => setLines(value))
    return () => { stream = undefined; resizeComposer = undefined }
  }, [])
  return <main {...stylex.props(styles.root, ...baselineTheme, scheme === 'light' ? lightTheme : embraceDarkTheme)}><section {...stylex.props(styles.frame)}><EmbraceVirtualConversation items={rows} renderItem={renderRow} estimatedRowHeight={80} /></section><form data-follow-composer aria-label="Composer" {...stylex.props(styles.composer)}><textarea aria-label="Message" rows={lines} readOnly value="Draft reply" {...stylex.props(styles.draft)} /></form></main>
}
const meta = { title: 'Fractal UI/Virtual conversation', component: VirtualStory, args: { scheme: 'dark' }, parameters: { layout: 'fullscreen' }, argTypes: { scheme: { options: ['dark', 'light'], control: 'radio' } } } satisfies Meta<typeof VirtualStory>
export default meta
type Story = StoryObj<typeof meta>
const pillName = 'Scroll to end'
const gap = (lane: HTMLElement) => lane.scrollHeight - lane.clientHeight - lane.scrollTop
// This package targets ES2022, where Promise.withResolvers is not available.
const settle = () => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve())))
const paintedAfter = <T,>(change: () => void, read: () => T) => new Promise<T>(resolve => requestAnimationFrame(() => {
  change()
  const channel = new MessageChannel()
  channel.port1.onmessage = () => { channel.port1.close(); channel.port2.close(); resolve(read()) }
  channel.port2.postMessage(null)
}))
async function ready(canvasElement: HTMLElement) {
  await document.fonts.ready
  const lane = within(canvasElement).getByTestId('transcript-scroll')
  await waitFor(() => expect(gap(lane)).toBeLessThanOrEqual(1))
  await settle()
  return { canvas: within(canvasElement), lane }
}
async function readerScrollsUp(lane: HTMLElement, by: number) {
  lane.dispatchEvent(new WheelEvent('wheel', { deltaY: -by, bubbles: true }))
  lane.scrollTop -= by
  lane.dispatchEvent(new Event('scroll'))
  await settle()
}
/** Control fault only for the kit controller; RAC's own row measurement remains real and synchronous. */
let resizeFault: 'none' | 'late' | 'skip' = 'none'
function faultyResize() {
  const Native = window.ResizeObserver
  const NativeMutation = window.MutationObserver
  resizeFault = 'none'
  window.ResizeObserver = class extends Native {
    constructor(callback: ResizeObserverCallback) {
      const controller = new Error().stack?.includes('ScrollController') === true
      super((entries, observer) => {
        if (!controller || resizeFault === 'none') callback(entries, observer)
        else if (resizeFault === 'late') requestAnimationFrame(() => callback(entries, observer))
      })
    }
  }
  window.MutationObserver = class extends NativeMutation {
    constructor(callback: MutationCallback) {
      const controller = new Error().stack?.includes('ScrollController') === true
      super((entries, observer) => {
        if (!controller || resizeFault === 'none') callback(entries, observer)
        else if (resizeFault === 'late') requestAnimationFrame(() => callback(entries, observer))
      })
    }
  }
  return () => { resizeFault = 'none'; window.ResizeObserver = Native; window.MutationObserver = NativeMutation }
}
async function provePinned(canvasElement: HTMLElement, fault = false) {
  const { lane } = await ready(canvasElement)
  if (fault) resizeFault = 'late'
  const before = lane.scrollHeight
  const painted: number[] = []
  for (let chunk = 0; chunk < 16; chunk++) painted.push(await paintedAfter(() => stream!(), () => gap(lane)))
  await expect(lane.scrollHeight - before, 'stream must grow the virtual lane').toBeGreaterThan(200)
  await expect(Math.max(...painted), `virtual streaming painted away from the end: ${painted.join(',')}`).toBeLessThanOrEqual(1)
  for (const lines of [8, 3, 12, 2]) {
    const beforeHeight = lane.clientHeight
    const painted = await paintedAfter(() => resizeComposer!(lines), () => ({ gap: gap(lane), height: lane.clientHeight }))
    await expect(Math.abs(painted.height - beforeHeight), 'composer must resize the virtual viewport').toBeGreaterThan(20)
    await expect(painted.gap, 'virtual viewport resize painted away from the end').toBeLessThanOrEqual(1)
  }
}
export const SynchronousResize: Story = { play: async ({ canvasElement }) => { await provePinned(canvasElement) } }
export const SynchronousResizeLight: Story = { ...SynchronousResize, args: { scheme: 'light' } }
export const FrameLateResizeControl: Story = { beforeEach: faultyResize, play: async ({ canvasElement }) => {
  await expect(provePinned(canvasElement, true)).rejects.toThrow(/painted away/)
} }
export const FrameLateResizeControlLight: Story = { ...FrameLateResizeControl, args: { scheme: 'light' } }
async function proveDetachedAnchor(canvasElement: HTMLElement, fault = false) {
  const { lane } = await ready(canvasElement)
  await readerScrollsUp(lane, 700)
  lane.dispatchEvent(new Event('scrollend'))
  await settle()
  const top = lane.getBoundingClientRect().top
  const entries = Array.from(lane.querySelectorAll<HTMLElement>('[data-embrace-entry-id]'))
  const line = entries.find(row => row.getBoundingClientRect().top >= top)!
  const key = line.dataset['embraceEntryId']!
  const above = entries.filter(row => row.getBoundingClientRect().bottom <= top).at(-1)!
  await expect(above, 'an overscanned row above the reader must be mounted').toBeDefined()
  const offset = () => lane.querySelector<HTMLElement>(`[data-embrace-entry-id="${key}"]`)!.getBoundingClientRect().top - lane.getBoundingClientRect().top
  const start = offset()
  if (fault) resizeFault = 'skip'
  for (const [label, change] of [
    ['growth below', () => stream!()],
    ['growth above', () => expanders.get(above.dataset['embraceEntryId']!)!(true)],
    ['shrink above', () => expanders.get(above.dataset['embraceEntryId']!)!(false)],
  ] as const) {
    const moved = await paintedAfter(change, () => Math.abs(offset() - start))
    await expect(moved, `${label} painted the virtual reading line elsewhere`).toBeLessThanOrEqual(1)
    await settle()
    await expect(Math.abs(offset() - start), `${label} moved the settled virtual reading line`).toBeLessThanOrEqual(1)
  }
}
export const DetachedGrowthAndShrink: Story = { play: async ({ canvasElement }) => { await proveDetachedAnchor(canvasElement) } }
export const DetachedGrowthAndShrinkLight: Story = { ...DetachedGrowthAndShrink, args: { scheme: 'light' } }
export const UncompensatedAnchorControl: Story = { beforeEach: faultyResize, play: async ({ canvasElement }) => {
  await expect(proveDetachedAnchor(canvasElement, true)).rejects.toThrow(/growth above painted/)
} }
export const UncompensatedAnchorControlLight: Story = { ...UncompensatedAnchorControl, args: { scheme: 'light' } }
async function provePill(canvasElement: HTMLElement, insideBand = false, activate = true) {
  const { canvas, lane } = await ready(canvasElement)
  await readerScrollsUp(lane, 20)
  if (insideBand) canvasElement.querySelector<HTMLButtonElement>('[aria-label="Scroll to end"]')!.hidden = false
  await expect(canvas.queryByRole('button', { name: pillName }), '20px is inside the virtual pill band').toBeNull()
  await readerScrollsUp(lane, 380)
  const pill = canvas.getByRole('button', { name: pillName })
  await expect(pill).toBeVisible()
  // No content change: reading existing history alone must offer the same pill as the DOM lane.
  const draft = canvas.getByRole('textbox', { name: 'Message' })
  draft.focus({ preventScroll: true })
  await userEvent.tab({ shift: true })
  await expect(document.activeElement).toBe(pill)
  if (activate) await userEvent.keyboard('{Enter}')
  await settle()
  await expect(gap(lane), 'keyboard pill activation must reach the virtual end').toBeLessThanOrEqual(1)
  await expect(canvas.queryByRole('button', { name: pillName })).toBeNull()
  await expect(document.activeElement).toBe(draft)
}
export const SharedPill: Story = { play: async ({ canvasElement }) => { await provePill(canvasElement) } }
export const SharedPillLight: Story = { ...SharedPill, args: { scheme: 'light' } }
export const InsideBandControl: Story = { play: async ({ canvasElement }) => { await expect(provePill(canvasElement, true)).rejects.toThrow(/20px is inside/) } }
export const InsideBandControlLight: Story = { ...InsideBandControl, args: { scheme: 'light' } }
export const PillNotActivatedControl: Story = { play: async ({ canvasElement }) => { await expect(provePill(canvasElement, false, false)).rejects.toThrow(/activation must reach/) } }
export const PillNotActivatedControlLight: Story = { ...PillNotActivatedControl, args: { scheme: 'light' } }
async function proveMomentum(canvasElement: HTMLElement, shortBy = 0) {
  const { canvas, lane } = await ready(canvasElement)
  await readerScrollsUp(lane, 600)
  await new Promise<void>(resolve => setTimeout(resolve, 400))
  resizeComposer!(8)
  await settle()
  lane.scrollTop = lane.scrollHeight - lane.clientHeight - shortBy
  // A momentum gesture can end before a deferred native scroll callback is delivered.
  lane.dispatchEvent(new Event('scrollend'))
  await settle()
  await expect(canvas.queryByRole('button', { name: pillName }), 'momentum at the virtual end must resume following').toBeNull()
  const painted = await paintedAfter(() => stream!(), () => gap(lane))
  await expect(painted, 'momentum re-follow must keep virtual streaming pinned').toBeLessThanOrEqual(1)
}
export const MomentumRefollows: Story = { play: async ({ canvasElement }) => { await proveMomentum(canvasElement) } }
export const MomentumRefollowsLight: Story = { ...MomentumRefollows, args: { scheme: 'light' } }
export const MomentumStopsShortControl: Story = { play: async ({ canvasElement }) => { await expect(proveMomentum(canvasElement, 60)).rejects.toThrow(/momentum at the virtual end/) } }
export const MomentumStopsShortControlLight: Story = { ...MomentumStopsShortControl, args: { scheme: 'light' } }
export const AllStates: Story = { render: args => <VirtualStory {...args} /> }
export const AllStatesLight: Story = { ...AllStates, args: { scheme: 'light' } }
const styles = stylex.create({
  root: { height: '100vh', width: '100%', display: 'flex', flexDirection: 'column', backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans },
  frame: { flex: '1 1 0', minHeight: 0 },
  row: { padding: s.lg, minHeight: g.controlMd, boxSizing: 'border-box' },
  text: { margin: 0, whiteSpace: 'pre-wrap', fontFamily: 'inherit', fontSize: t.bodySize, lineHeight: t.bodyLeading },
  composer: { flexShrink: 0, display: 'flex', padding: s.lg },
  draft: { flex: '1 1 auto', resize: 'none', color: ink.fg, backgroundColor: surface.controlFill, fontFamily: t.fontSans, fontSize: t.bodySize, lineHeight: t.bodyLeading },
})
