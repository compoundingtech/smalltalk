import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { expect, userEvent, waitFor, within } from 'storybook/test'
import { Button, Form, TextField, TextArea } from 'react-aria-components'
import { EmbraceScrollViewport, ViewportStore, ViewportStoreContext } from './assistant-ui/EmbraceScrollViewport'
import { EmbraceVirtualConversation } from './assistant-ui/EmbraceVirtualConversation'
import { baselineTheme } from './assistant-ui/neutral-theme'
import { lightTheme, type Scheme } from './assistant-ui/composition-theme'
import { surfaceVars as surface, textVars as ink, borderVars as border, accentVars as accent, spaceVars as s, typeVars as t, geometryVars as g, radiusVars as r } from './assistant-ui/composition-tokens.stylex'

const frame = () => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve())))
const image = 'data:image/svg+xml,' + encodeURIComponent('<svg xmlns="http://www.w3.org/2000/svg" width="640" height="360"><rect width="640" height="360" fill="#888"/><path d="M0 300L180 100L350 240L480 80L640 300" fill="none" stroke="#ddd" stroke-width="12"/></svg>')
function FollowStory({ scheme = 'dark', virtual = false }: { scheme?: Scheme; virtual?: boolean }) {
  const root = React.useRef<HTMLElement>(null)
  const [chunks, setChunks] = React.useState(0)
  const [streamChunk, setStreamChunk] = React.useState(0)
  const [streaming, setStreaming] = React.useState(false)
  const [expanded, setExpanded] = React.useState(false)
  const [toolExpanded, setToolExpanded] = React.useState(false)
  const [expandedRow, setExpandedRow] = React.useState<string>()
  const [older, setOlder] = React.useState(false)
  const [thread, setThread] = React.useState('one')
  const [send, setSend] = React.useState(0)
  const [store] = React.useState(() => new ViewportStore())
  const [inspections, setInspections] = React.useState(0)
  const [draft, setDraft] = React.useState('')
  const [invalidPill, setInvalidPill] = React.useState(false)
  const rows = React.useMemo(() => [
    ...(older ? Array.from({ length: 8 }, (_, index) => ({ id: `older-${index}`, text: `Earlier observation ${index + 1}: the history anchor stays on the reader's line.` })) : []),
    ...Array.from({ length: 40 }, (_, index) => ({ id: `entry-${index}`, text: `Message ${index + 1}: the transcript follows the live conversation unless the reader chooses to read earlier messages.` })),
    { id: 'latest', text: `Latest reply in conversation ${thread}.\n${'Observed streaming chunk adds a new line to the conversation.\n'.repeat(chunks)}` },
    ...(send > 0 ? [{ id: `own-${send}`, text: `Own message ${send}: preserve the reader's draft and follow the pending turn.` }] : []),
  ], [chunks, older, thread, send])
  const renderRow = (row: typeof rows[number]) => <article data-item-id={row.id} {...stylex.props(styles.row)}>
    {expandedRow === row.id && <pre>{'An expanded observation above the reader line.\n'.repeat(8)}</pre>}
    <p data-follow-anchor {...stylex.props(styles.paragraph)}>{row.text}</p>
    {row.id === 'latest' && <><img src={image} alt="Conversation attachment" {...stylex.props(styles.image, expanded && styles.expandedImage)} /><Button {...stylex.props(styles.button)} onPress={() => setInspections(value => value + 1)}>Inspect latest</Button><span> Inspected {inspections} times</span>{toolExpanded && <pre>{'Observed tool output: completed one measured operation.\n'.repeat(24)}</pre>}</>}
  </article>
  return <ViewportStoreContext.Provider value={store}><main ref={root} {...stylex.props(styles.root, ...baselineTheme, scheme === 'light' && lightTheme)}>
    <div {...stylex.props(styles.toolbar)}>
      <Button isDisabled={streaming} {...stylex.props(styles.button)} onPress={async () => {
        setStreaming(true)
        setStreamChunk(0)
        for (let chunk = 1; chunk <= 50; chunk++) { setChunks(value => value + 1); setStreamChunk(chunk); await frame() }
        setStreaming(false)
      }}>Stream 50 chunks</Button>
      <Button {...stylex.props(styles.button)} onPress={() => setExpanded(true)}>Expand image</Button>
      <Button {...stylex.props(styles.button)} onPress={() => setToolExpanded(true)}>Expand tool</Button>
      <Button {...stylex.props(styles.button)} onPress={() => setOlder(true)}>Insert earlier history</Button>
      <Button {...stylex.props(styles.button)} onPress={() => {
        const viewport = root.current?.querySelector<HTMLElement>('[data-testid="transcript-scroll"]')
        if (viewport === undefined || viewport === null) return
        const top = viewport.getBoundingClientRect().top
        const line = Array.from(viewport.querySelectorAll<HTMLElement>('[data-follow-anchor]')).find(node => node.getBoundingClientRect().bottom > top)
        setExpandedRow(line?.closest<HTMLElement>('[data-item-id]')?.dataset['itemId'])
      }}>Expand above reading line</Button>
      <Button {...stylex.props(styles.button)} onPress={() => setSend(value => value + 1)}>Send own message</Button>
      <Button {...stylex.props(styles.button)} onPress={() => setThread(value => value === 'one' ? 'two' : 'one')}>Switch thread</Button>
      <Button {...stylex.props(styles.button)} onPress={() => setInvalidPill(true)}>Inject invalid pill</Button>
      <span data-testid="stream-progress">{streaming ? `Streaming ${streamChunk}` : `Complete ${streamChunk}`}</span>
    </div>
    {virtual ? <EmbraceVirtualConversation items={rows} renderItem={renderRow} anchorKey={thread} scrollToBottomKey={String(send)} isRunning={streaming} /> : <EmbraceScrollViewport items={rows} stateKey={thread} scrollToBottomKey={String(send)} isRunning={streaming} data-testid="transcript-scroll" aria-label="Conversation history" tabIndex={-1} {...stylex.props(styles.viewport)} contentProps={stylex.props(styles.rows)}>{rows.map(row => <React.Fragment key={row.id}>{renderRow(row)}</React.Fragment>)}</EmbraceScrollViewport>}
    <Form data-follow-composer {...stylex.props(styles.composer)} onSubmit={event => event.preventDefault()}><TextField aria-label="Message" value={draft} onChange={setDraft}><TextArea {...stylex.props(styles.input)} onKeyDown={event => { if (event.key === 'Enter' && !event.shiftKey) { event.preventDefault(); setSend(value => value + 1); setDraft('') } }} /></TextField></Form>
    {invalidPill && <Button aria-label="Scroll to end" data-testid="invalid-pill" {...stylex.props(styles.invalid)}>Scroll to end</Button>}
  </main></ViewportStoreContext.Provider>
}
const meta = { title: 'Fractal UI/Transcript follow', component: FollowStory, args: { scheme: 'dark', virtual: false }, parameters: { layout: 'fullscreen' }, argTypes: { scheme: { options: ['dark', 'light'], control: 'radio' }, virtual: { control: 'boolean' } } } satisfies Meta<typeof FollowStory>
export default meta
type Story = StoryObj<typeof meta>
const jumpName = 'Scroll to end'
const jumpNames = /^(Scroll to end|New messages ↓)$/
async function ready(canvasElement: HTMLElement) {
  await document.fonts.ready
  const canvas = within(canvasElement)
  const viewport = canvas.getByTestId('transcript-scroll')
  await frame()
  await waitFor(() => expect(viewport.scrollHeight - viewport.clientHeight - viewport.scrollTop).toBeLessThanOrEqual(2))
  return { canvas, viewport }
}
async function scrollByReader(viewport: HTMLElement, top: number, key?: string) {
  viewport.focus()
  viewport.dispatchEvent(key === undefined ? new WheelEvent('wheel', { deltaY: top - viewport.scrollTop }) : new KeyboardEvent('keydown', { key, bubbles: true }))
  viewport.scrollTop = top
  viewport.dispatchEvent(new Event('scroll'))
  await frame()
}
async function expectAttached(canvasElement: HTMLElement, viewport: HTMLElement) {
  await waitFor(() => expect(viewport.scrollHeight - viewport.clientHeight - viewport.scrollTop).toBeLessThanOrEqual(2))
  await expect(within(canvasElement).queryByRole('button', { name: jumpNames })).toBeNull()
}
async function jumpAfterReading(canvasElement: HTMLElement, viewport: HTMLElement) {
  await scrollByReader(viewport, 240)
  return await waitFor(() => within(canvasElement).getByRole('button', { name: jumpName }))
}
function readingLine(viewport: HTMLElement) {
  const top = viewport.getBoundingClientRect().top
  const line = Array.from(viewport.querySelectorAll<HTMLElement>('[data-follow-anchor]')).find(node => node.getBoundingClientRect().bottom > top)!
  return { line, offset: line.getBoundingClientRect().top - top }
}

export const OpenAtBottom: Story = { play: async ({ canvasElement }) => {
  const { viewport } = await ready(canvasElement)
  await expectAttached(canvasElement, viewport)
} }
export const AtEndNoAffordance: Story = { play: async ({ canvasElement }) => {
  const { viewport } = await ready(canvasElement)
  viewport.focus()
  for (let sample = 0; sample < 12; sample++) { await frame(); await expectAttached(canvasElement, viewport) }
} }
export const NoDocumentScroll: Story = { play: async ({ canvasElement }) => {
  await ready(canvasElement)
  await expect(document.scrollingElement!.scrollHeight).toBeLessThanOrEqual(window.innerHeight + 1)
} }
export const ScrollUpDetaches: Story = { play: async ({ canvasElement }) => {
  const { canvas, viewport } = await ready(canvasElement)
  const jump = await jumpAfterReading(canvasElement, viewport)
  await expect(canvas.getAllByRole('button', { name: jumpName })).toHaveLength(1)
  await expect(canvas.getByRole('status')).toHaveTextContent('Scroll to end is available')
  const bounds = jump.getBoundingClientRect(), composer = canvasElement.querySelector('[data-follow-composer]')!.getBoundingClientRect(), lane = viewport.getBoundingClientRect()
  await expect(bounds.height).toBe(24)
  await expect(Math.abs(composer.top - bounds.bottom - 18)).toBeLessThanOrEqual(1)
  await expect(Math.abs(bounds.left + bounds.width / 2 - lane.left - lane.width / 2)).toBeLessThanOrEqual(1)
} }
export const ReattachThreshold: Story = { play: async ({ canvasElement }) => {
  const { canvas, viewport } = await ready(canvasElement)
  await jumpAfterReading(canvasElement, viewport)
  await scrollByReader(viewport, viewport.scrollHeight - viewport.clientHeight - 45)
  await expect(canvas.getByRole('button', { name: jumpName })).toBeVisible()
  await scrollByReader(viewport, viewport.scrollHeight - viewport.clientHeight - 40)
  await expectAttached(canvasElement, viewport)
} }
export const AffordanceReattaches: Story = { play: async ({ canvasElement }) => {
  const { viewport } = await ready(canvasElement)
  const jump = await jumpAfterReading(canvasElement, viewport)
  const before = viewport.scrollTop
  const from = performance.now()
  await userEvent.click(jump)
  await expect(within(canvasElement).queryByRole('button', { name: jumpName })).toBeNull()
  let intermediate = 0
  for (let sample = 0; sample < 40; sample++) {
    await new Promise(resolve => requestAnimationFrame(resolve))
    const distance = viewport.scrollHeight - viewport.clientHeight - viewport.scrollTop
    if (viewport.scrollTop > before + 2 && distance > 40) intermediate++
    if (distance <= 2) break
  }
  await expect(intermediate).toBeGreaterThanOrEqual(3)
  await expect(performance.now() - from).toBeLessThan(1000)
  await expectAttached(canvasElement, viewport)
} }
export const AffordanceKeyboard: Story = { play: async ({ canvasElement }) => {
  const { canvas, viewport } = await ready(canvasElement)
  const jump = await jumpAfterReading(canvasElement, viewport)
  await userEvent.click(canvas.getByRole('textbox', { name: 'Message' }))
  await userEvent.tab({ shift: true })
  await expect(jump).toHaveFocus()
  const css = getComputedStyle(jump)
  await expect(css.outlineStyle).toBe('solid')
  await expect(parseFloat(css.outlineWidth)).toBeGreaterThanOrEqual(3)
  await userEvent.keyboard('{Enter}')
  await expectAttached(canvasElement, viewport)
  await expect(document.activeElement).not.toBe(jump)
} }
export const KeyboardReattaches: Story = { play: async ({ canvasElement }) => {
  const { canvas, viewport } = await ready(canvasElement)
  await userEvent.click(canvas.getByRole('button', { name: 'Inspect latest' }))
  await expectAttached(canvasElement, viewport)
  await expect(viewport.tabIndex).toBe(-1)
  await scrollByReader(viewport, 240, 'PageUp')
  await waitFor(() => expect(canvas.getByRole('button', { name: jumpName })).toBeVisible())
  await scrollByReader(viewport, viewport.scrollHeight, 'End')
  await expectAttached(canvasElement, viewport)
  await scrollByReader(viewport, 240, 'Home')
  await scrollByReader(viewport, viewport.scrollHeight, 'PageDown')
  await expectAttached(canvasElement, viewport)
} }
export const PinnedStreaming: Story = { play: async ({ canvasElement }) => {
  const { canvas, viewport } = await ready(canvasElement)
  await userEvent.click(canvas.getByRole('button', { name: 'Inspect latest' }))
  const height = viewport.scrollHeight
  await userEvent.click(canvas.getByRole('button', { name: 'Stream 50 chunks' }))
  let awayFrom: number | undefined, worstAway = 0
  while (!canvas.getByTestId('stream-progress').textContent?.includes('Complete 50')) {
    await new Promise(resolve => requestAnimationFrame(resolve))
    await expect(canvas.queryByRole('button', { name: jumpNames })).toBeNull()
    if (viewport.scrollHeight - viewport.clientHeight - viewport.scrollTop > 40) {
      awayFrom ??= performance.now()
      worstAway = Math.max(worstAway, performance.now() - awayFrom)
    } else awayFrom = undefined
  }
  await expect(worstAway).toBeLessThanOrEqual(500)
  await expect(viewport.scrollHeight - height).toBeGreaterThan(40)
  await expectAttached(canvasElement, viewport)
} }
export const DetachedHolds: Story = { play: async ({ canvasElement }) => {
  const { canvas, viewport } = await ready(canvasElement)
  await jumpAfterReading(canvasElement, viewport)
  const { line, offset } = readingLine(viewport)
  await userEvent.click(canvas.getByRole('button', { name: 'Stream 50 chunks' }))
  while (!canvas.getByTestId('stream-progress').textContent?.includes('Complete 50')) {
    await frame()
    await expect(Math.abs(line.getBoundingClientRect().top - viewport.getBoundingClientRect().top - offset)).toBeLessThanOrEqual(2)
    await expect(canvas.getAllByRole('button', { name: jumpName })).toHaveLength(1)
  }
} }
export const AnchorAbove: Story = { play: async ({ canvasElement }) => {
  const { canvas, viewport } = await ready(canvasElement)
  await scrollByReader(viewport, 240)
  const { line, offset } = readingLine(viewport)
  await userEvent.click(canvas.getByRole('button', { name: 'Insert earlier history' }))
  await frame()
  await expect(Math.abs(line.getBoundingClientRect().top - viewport.getBoundingClientRect().top - offset)).toBeLessThanOrEqual(2)
  await userEvent.click(canvas.getByRole('button', { name: 'Expand above reading line' }))
  await frame()
  await expect(Math.abs(line.getBoundingClientRect().top - viewport.getBoundingClientRect().top - offset)).toBeLessThanOrEqual(2)
} }
export const OwnSendReattaches: Story = { play: async ({ canvasElement }) => {
  const { canvas, viewport } = await ready(canvasElement)
  await jumpAfterReading(canvasElement, viewport)
  const from = performance.now()
  await userEvent.click(canvas.getByRole('button', { name: 'Send own message' }))
  await expectAttached(canvasElement, viewport)
  await expect(performance.now() - from).toBeLessThanOrEqual(250)
} }
export const ThreadSwitch: Story = { play: async ({ canvasElement }) => {
  const { canvas, viewport } = await ready(canvasElement)
  await jumpAfterReading(canvasElement, viewport)
  const { line, offset } = readingLine(viewport)
  const text = line.textContent
  await userEvent.click(canvas.getByRole('button', { name: 'Switch thread' }))
  await expectAttached(canvasElement, canvas.getByTestId('transcript-scroll'))
  await userEvent.click(canvas.getByRole('button', { name: 'Switch thread' }))
  await frame()
  const returned = Array.from(canvas.getByTestId('transcript-scroll').querySelectorAll<HTMLElement>('[data-follow-anchor]')).find(node => node.textContent === text)!
  await expect(Math.abs(returned.getBoundingClientRect().top - viewport.getBoundingClientRect().top - offset)).toBeLessThanOrEqual(2)
  const jump = await waitFor(() => canvas.getByRole('button', { name: jumpName }))
  await userEvent.click(jump)
  await expectAttached(canvasElement, viewport)
  await userEvent.click(canvas.getByRole('button', { name: 'Switch thread' }))
  await frame()
  await userEvent.click(canvas.getByRole('button', { name: 'Switch thread' }))
  await expectAttached(canvasElement, canvas.getByTestId('transcript-scroll'))
} }
export const UpInsideBandStillDetaches: Story = { play: async ({ canvasElement }) => {
  const { canvas, viewport } = await ready(canvasElement)
  await scrollByReader(viewport, viewport.scrollHeight - viewport.clientHeight - 25)
  await expect(canvas.queryByRole('button', { name: jumpName })).toBeNull()
  const top = viewport.scrollTop
  await userEvent.click(canvas.getByRole('button', { name: 'Expand image' }))
  await frame()
  await expect(Math.abs(viewport.scrollTop - top)).toBeLessThanOrEqual(2)
  await waitFor(() => expect(canvas.getByRole('button', { name: jumpName })).toBeVisible())
} }
export const BottomReattaches: Story = { ...ReattachThreshold }
export const NoPillAtBottomStreaming: Story = { ...PinnedStreaming }
export const NoDocumentScrollLight: Story = { ...NoDocumentScroll, args: { scheme: 'light' } }
export const PinnedImageExpansion: Story = { play: async ({ canvasElement }) => {
  const { canvas, viewport } = await ready(canvasElement)
  await userEvent.click(canvas.getByRole('button', { name: 'Inspect latest' }))
  const before = viewport.scrollHeight
  await userEvent.click(canvas.getByRole('button', { name: 'Expand image' }))
  await waitFor(() => expect(viewport.scrollHeight - before).toBeGreaterThan(200))
  await expectAttached(canvasElement, viewport)
} }
export const PinnedToolExpansion: Story = { play: async ({ canvasElement }) => {
  const { canvas, viewport } = await ready(canvasElement)
  await userEvent.click(canvas.getByRole('button', { name: 'Inspect latest' }))
  const before = viewport.scrollHeight
  await userEvent.click(canvas.getByRole('button', { name: 'Expand tool' }))
  await waitFor(() => expect(viewport.scrollHeight - before).toBeGreaterThan(200))
  await expectAttached(canvasElement, viewport)
} }
export const ProgrammaticScrollDoesNotDetach: Story = { play: async ({ canvasElement }) => {
  const { canvas, viewport } = await ready(canvasElement)
  viewport.scrollTop = 240
  viewport.dispatchEvent(new Event('scroll'))
  await frame()
  await userEvent.click(canvas.getByRole('button', { name: 'Expand image' }))
  await expectAttached(canvasElement, viewport)
} }
export const ControlledNegative: Story = { play: async ({ canvasElement }) => {
  const { canvas, viewport } = await ready(canvasElement)
  await expectAttached(canvasElement, viewport)
  await userEvent.click(canvas.getByRole('button', { name: 'Inject invalid pill' }))
  await expect(canvas.getByTestId('invalid-pill')).toBeVisible()
  await expect(expectAttached(canvasElement, viewport), 'the at-end assertion must reject an injected visible pill').rejects.toThrow()
} }
export const AllStates: Story = { render: args => <FollowStory {...args} /> }
export const OpenAtBottomLight: Story = { ...OpenAtBottom, args: { scheme: 'light' } }
export const AtEndNoAffordanceLight: Story = { ...AtEndNoAffordance, args: { scheme: 'light' } }
export const ScrollUpDetachesLight: Story = { ...ScrollUpDetaches, args: { scheme: 'light' } }
export const ReattachThresholdLight: Story = { ...ReattachThreshold, args: { scheme: 'light' } }
export const AffordanceReattachesLight: Story = { ...AffordanceReattaches, args: { scheme: 'light' } }
export const AffordanceKeyboardLight: Story = { ...AffordanceKeyboard, args: { scheme: 'light' } }
export const KeyboardReattachesLight: Story = { ...KeyboardReattaches, args: { scheme: 'light' } }
export const PinnedStreamingLight: Story = { ...PinnedStreaming, args: { scheme: 'light' } }
export const DetachedHoldsLight: Story = { ...DetachedHolds, args: { scheme: 'light' } }
export const AnchorAboveLight: Story = { ...AnchorAbove, args: { scheme: 'light' } }
export const OwnSendReattachesLight: Story = { ...OwnSendReattaches, args: { scheme: 'light' } }
export const ThreadSwitchLight: Story = { ...ThreadSwitch, args: { scheme: 'light' } }
export const UpInsideBandStillDetachesLight: Story = { ...UpInsideBandStillDetaches, args: { scheme: 'light' } }
export const BottomReattachesLight: Story = { ...BottomReattaches, args: { scheme: 'light' } }
export const NoPillAtBottomStreamingLight: Story = { ...NoPillAtBottomStreaming, args: { scheme: 'light' } }
export const PinnedImageExpansionLight: Story = { ...PinnedImageExpansion, args: { scheme: 'light' } }
export const PinnedToolExpansionLight: Story = { ...PinnedToolExpansion, args: { scheme: 'light' } }
export const ProgrammaticScrollDoesNotDetachLight: Story = { ...ProgrammaticScrollDoesNotDetach, args: { scheme: 'light' } }
export const ControlledNegativeLight: Story = { ...ControlledNegative, args: { scheme: 'light' } }
export const VirtualPinnedStreaming: Story = { ...PinnedStreaming, args: { virtual: true } }
export const VirtualScrollUpDetaches: Story = { ...ScrollUpDetaches, args: { virtual: true } }
export const VirtualThreadSwitch: Story = { ...ThreadSwitch, args: { virtual: true } }
const styles = stylex.create({
  root: { display: 'flex', flexDirection: 'column', height: '100vh', minHeight: 0, boxSizing: 'border-box', padding: s.lg, backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans },
  toolbar: { display: 'flex', flexWrap: 'wrap', alignItems: 'center', gap: s.md, marginBottom: s.md, flexShrink: 0 },
  button: { minHeight: g.controlMd, paddingInline: s.md, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, backgroundColor: surface.controlFill, color: ink.fg, fontSize: t.metaSize, cursor: 'pointer', ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary } },
  viewport: { flex: '1 1 0', minHeight: 0, overflowY: 'auto' }, rows: { display: 'flex', flexDirection: 'column', gap: s.lg },
  row: { padding: s.lg, minHeight: g.resourceCard, borderBottomWidth: g.hairline, borderBottomStyle: 'solid', borderBottomColor: border.border, fontSize: t.bodySize, lineHeight: t.bodyLeading, whiteSpace: 'pre-wrap' }, paragraph: { margin: 0 },
  image: { display: 'block', width: '100%', maxWidth: 640, height: 80, objectFit: 'cover', marginBlock: s.md }, expandedImage: { height: 360 },
  composer: { flexShrink: 0, padding: s.md, borderRadius: r.control, backgroundColor: surface.glassFill, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.glassBorder },
  input: { width: '100%', minHeight: g.controlLg, boxSizing: 'border-box', borderWidth: 0, backgroundColor: surface.transparent, color: ink.fg, fontFamily: t.fontSans, fontSize: t.uiSize, resize: 'none', ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary } },
  invalid: { position: 'fixed', bottom: 120, left: '50%', backgroundColor: surface.raised, color: ink.fg },
})
