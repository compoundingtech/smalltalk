import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { expect, userEvent, waitFor, within } from 'storybook/test'
import { Button } from 'react-aria-components'
import { EmbraceScrollViewport, ViewportStore, ViewportStoreContext } from './assistant-ui/EmbraceScrollViewport'
import { EmbraceVirtualConversation } from './assistant-ui/EmbraceVirtualConversation'
import { baselineTheme } from './assistant-ui/neutral-theme'
import { lightTheme, type Scheme } from './assistant-ui/composition-theme'
import { surfaceVars as surface, textVars as ink, borderVars as border, accentVars as accent, spaceVars as s, typeVars as t, geometryVars as g } from './assistant-ui/composition-tokens.stylex'

const frame = () => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve())))
const image = 'data:image/svg+xml,' + encodeURIComponent('<svg xmlns="http://www.w3.org/2000/svg" width="640" height="360"><rect width="640" height="360" fill="#888"/><path d="M0 300L180 100L350 240L480 80L640 300" fill="none" stroke="#ddd" stroke-width="12"/></svg>')

function FollowStory({ scheme = 'dark', virtual = false }: { scheme?: Scheme; virtual?: boolean }) {
  const [chunks, setChunks] = React.useState(0)
  const [streaming, setStreaming] = React.useState(false)
  const [expanded, setExpanded] = React.useState(false)
  const [older, setOlder] = React.useState(false)
  const [thread, setThread] = React.useState('one')
  const [send, setSend] = React.useState(0)
  const [store] = React.useState(() => new ViewportStore())
  const [inspections, setInspections] = React.useState(0)
  const rows = React.useMemo(() => [
    ...(older ? Array.from({ length: 8 }, (_, index) => ({ id: `older-${index}`, text: `Earlier observation ${index + 1}: the history anchor stays on the reader's line.` })) : []),
    ...Array.from({ length: 40 }, (_, index) => ({ id: `entry-${index}`, text: `Message ${index + 1}: the transcript follows the live conversation unless the reader chooses to read earlier messages.` })),
    { id: 'latest', text: `Latest reply in conversation ${thread}.\n${'Observed streaming chunk adds a new line to the conversation.\n'.repeat(chunks)}` },
  ], [chunks, older, thread])
  const renderRow = (row: typeof rows[number]) => <article data-item-id={row.id} {...stylex.props(styles.row)}>{row.text}{row.id === 'latest' && <><img src={image} alt="Conversation attachment" {...stylex.props(styles.image, expanded && styles.expandedImage)} /><Button {...stylex.props(styles.button)} onPress={() => setInspections(value => value + 1)}>Inspect latest</Button><span> Inspected {inspections} times</span></>}</article>
  return <ViewportStoreContext.Provider value={store}><main {...stylex.props(styles.root, ...baselineTheme, scheme === 'light' && lightTheme)}>
    <div {...stylex.props(styles.toolbar)}>
      <Button isDisabled={streaming} {...stylex.props(styles.button)} onPress={async () => {
        setStreaming(true)
        for (let chunk = 1; chunk <= 50; chunk++) { setChunks(chunk); await frame() }
        setStreaming(false)
      }}>Stream 50 chunks</Button>
      <Button {...stylex.props(styles.button)} onPress={() => setExpanded(true)}>Expand image</Button>
      <Button {...stylex.props(styles.button)} onPress={() => setOlder(true)}>Insert earlier history</Button>
      <Button {...stylex.props(styles.button)} onPress={() => setSend(value => value + 1)}>Send own message</Button>
      <Button {...stylex.props(styles.button)} onPress={() => setThread(value => value === 'one' ? 'two' : 'one')}>Switch thread</Button>
      <span data-testid="stream-progress">{streaming ? `Streaming ${chunks}` : `Complete ${chunks}`}</span>
    </div>
    {virtual ? <EmbraceVirtualConversation items={rows} renderItem={renderRow} anchorKey={thread} scrollToBottomKey={String(send)} /> : <EmbraceScrollViewport items={rows} stateKey={thread} scrollToBottomKey={String(send)} data-testid="transcript-scroll" aria-label="Conversation history" tabIndex={0} {...stylex.props(styles.viewport)} contentProps={stylex.props(styles.rows)}>{rows.map(row => <React.Fragment key={row.id}>{renderRow(row)}</React.Fragment>)}</EmbraceScrollViewport>}
  </main></ViewportStoreContext.Provider>
}
const meta = { title: 'Fractal UI/Transcript follow', component: FollowStory, args: { scheme: 'dark', virtual: false }, parameters: { layout: 'fullscreen' }, argTypes: { scheme: { options: ['dark', 'light'], control: 'radio' }, virtual: { control: 'boolean' } } } satisfies Meta<typeof FollowStory>
export default meta
type Story = StoryObj<typeof meta>
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
  await expect(within(canvasElement).queryByRole('button', { name: 'New messages ↓' })).toBeNull()
}

export const OpenAtBottom: Story = { play: async ({ canvasElement }) => {
  const { viewport } = await ready(canvasElement)
  await expectAttached(canvasElement, viewport)
} }
export const PinnedStreaming: Story = { play: async ({ canvasElement }) => {
  const { canvas, viewport } = await ready(canvasElement)
  // Focusing and pressing a row action must not detach: this control fails on the base.
  await userEvent.click(canvas.getByRole('button', { name: 'Inspect latest' }))
  await userEvent.click(canvas.getByRole('button', { name: 'Stream 50 chunks' }))
  for (let chunk = 1; chunk <= 50; chunk++) {
    await waitFor(() => expect(canvas.getByTestId('stream-progress')).not.toHaveTextContent('Complete 0'))
    await frame()
    await expect(viewport.scrollHeight - viewport.clientHeight - viewport.scrollTop).toBeLessThanOrEqual(2)
  }
  await waitFor(() => expect(canvas.getByTestId('stream-progress')).toHaveTextContent('Complete 50'))
  await expectAttached(canvasElement, viewport)
} }
export const PinnedImageExpansion: Story = { play: async ({ canvasElement }) => {
  const { canvas, viewport } = await ready(canvasElement)
  await userEvent.click(canvas.getByRole('button', { name: 'Inspect latest' }))
  const before = viewport.scrollHeight
  await userEvent.click(canvas.getByRole('button', { name: 'Expand image' }))
  await waitFor(() => expect(viewport.scrollHeight - before).toBeGreaterThan(200))
  await frame()
  await expectAttached(canvasElement, viewport)
} }
export const ScrollUpDetaches: Story = { play: async ({ canvasElement }) => {
  const { canvas, viewport } = await ready(canvasElement)
  await scrollByReader(viewport, 240)
  await expect(canvas.getAllByRole('button', { name: 'New messages ↓' })).toHaveLength(1)
  await expect(canvas.getByRole('status')).toHaveTextContent('Jump to latest is available')
  const anchor = Array.from(viewport.querySelectorAll<HTMLElement>('[data-item-id]')).find(row => row.getBoundingClientRect().bottom > viewport.getBoundingClientRect().top)!
  const offset = anchor.getBoundingClientRect().top - viewport.getBoundingClientRect().top
  await userEvent.click(canvas.getByRole('button', { name: 'Insert earlier history' }))
  await frame()
  await expect(Math.abs(anchor.getBoundingClientRect().top - viewport.getBoundingClientRect().top - offset)).toBeLessThanOrEqual(2)
} }
export const AffordanceReattaches: Story = { play: async ({ canvasElement }) => {
  const { canvas, viewport } = await ready(canvasElement)
  await scrollByReader(viewport, 240)
  await userEvent.click(canvas.getByRole('button', { name: 'New messages ↓' }))
  await frame()
  await expectAttached(canvasElement, viewport)
  await userEvent.click(canvas.getByRole('button', { name: 'Expand image' }))
  await frame()
  await expectAttached(canvasElement, viewport)
} }
export const BottomReattaches: Story = { play: async ({ canvasElement }) => {
  const { viewport } = await ready(canvasElement)
  await scrollByReader(viewport, 240)
  await scrollByReader(viewport, viewport.scrollHeight)
  await expectAttached(canvasElement, viewport)
} }
export const OwnSendReattaches: Story = { play: async ({ canvasElement }) => {
  const { canvas, viewport } = await ready(canvasElement)
  await scrollByReader(viewport, 240)
  await userEvent.click(canvas.getByRole('button', { name: 'Send own message' }))
  await frame()
  await expectAttached(canvasElement, viewport)
} }
export const NoPillAtBottomStreaming: Story = { play: async ({ canvasElement }) => {
  const { canvas, viewport } = await ready(canvasElement)
  viewport.focus()
  await userEvent.click(canvas.getByRole('button', { name: 'Stream 50 chunks' }))
  while (!canvas.getByTestId('stream-progress').textContent?.includes('Complete 50')) {
    await frame()
    await expect(canvas.queryByRole('button', { name: 'New messages ↓' })).toBeNull()
  }
  await expectAttached(canvasElement, viewport)
} }
export const ThreadSwitchBackAtBottom: Story = { play: async ({ canvasElement }) => {
  const { canvas, viewport } = await ready(canvasElement)
  await scrollByReader(viewport, 240)
  await userEvent.click(canvas.getByRole('button', { name: 'Switch thread' }))
  await frame()
  await expectAttached(canvasElement, canvas.getByTestId('transcript-scroll'))
  await scrollByReader(canvas.getByTestId('transcript-scroll'), 320)
  await userEvent.click(canvas.getByRole('button', { name: 'Switch thread' }))
  await frame()
  await expectAttached(canvasElement, canvas.getByTestId('transcript-scroll'))
} }
export const KeyboardReattaches: Story = { play: async ({ canvasElement }) => {
  const { canvas, viewport } = await ready(canvasElement)
  await scrollByReader(viewport, 240, 'PageUp')
  const jump = canvas.getByRole('button', { name: 'New messages ↓' })
  jump.focus()
  await userEvent.keyboard('{Enter}')
  await frame()
  await expectAttached(canvasElement, viewport)
  await scrollByReader(viewport, 240, 'Home')
  await scrollByReader(viewport, viewport.scrollHeight, 'End')
  await expectAttached(canvasElement, viewport)
  await scrollByReader(viewport, viewport.scrollHeight - viewport.clientHeight, 'PageUp')
  await scrollByReader(viewport, viewport.scrollHeight, 'PageDown')
  await expectAttached(canvasElement, viewport)
} }
export const ProgrammaticScrollDoesNotDetach: Story = { play: async ({ canvasElement }) => {
  const { canvas, viewport } = await ready(canvasElement)
  viewport.scrollTop = 240
  viewport.dispatchEvent(new Event('scroll'))
  await frame()
  await userEvent.click(canvas.getByRole('button', { name: 'Expand image' }))
  await frame()
  await expectAttached(canvasElement, viewport)
} }
export const AllStates: Story = { render: args => <FollowStory {...args} /> }
export const OpenAtBottomLight: Story = { ...OpenAtBottom, args: { scheme: 'light' } }
export const PinnedStreamingLight: Story = { ...PinnedStreaming, args: { scheme: 'light' } }
export const PinnedImageExpansionLight: Story = { ...PinnedImageExpansion, args: { scheme: 'light' } }
export const ScrollUpDetachesLight: Story = { ...ScrollUpDetaches, args: { scheme: 'light' } }
export const AffordanceReattachesLight: Story = { ...AffordanceReattaches, args: { scheme: 'light' } }
export const BottomReattachesLight: Story = { ...BottomReattaches, args: { scheme: 'light' } }
export const OwnSendReattachesLight: Story = { ...OwnSendReattaches, args: { scheme: 'light' } }
export const NoPillAtBottomStreamingLight: Story = { ...NoPillAtBottomStreaming, args: { scheme: 'light' } }
export const ThreadSwitchBackAtBottomLight: Story = { ...ThreadSwitchBackAtBottom, args: { scheme: 'light' } }
export const KeyboardReattachesLight: Story = { ...KeyboardReattaches, args: { scheme: 'light' } }
export const ProgrammaticScrollDoesNotDetachLight: Story = { ...ProgrammaticScrollDoesNotDetach, args: { scheme: 'light' } }
export const VirtualPinnedStreaming: Story = { ...PinnedStreaming, args: { virtual: true } }
export const VirtualScrollUpDetaches: Story = { ...ScrollUpDetaches, args: { virtual: true } }
const styles = stylex.create({
  root: { display: 'flex', flexDirection: 'column', height: '100vh', minHeight: 0, boxSizing: 'border-box', padding: s.lg, gap: s.md, backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans },
  toolbar: { display: 'flex', flexWrap: 'wrap', alignItems: 'center', gap: s.md, flexShrink: 0 },
  button: { minHeight: g.controlMd, paddingInline: s.md, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, backgroundColor: surface.controlFill, color: ink.fg, fontSize: t.metaSize, cursor: 'pointer', ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary } },
  viewport: { flex: '1 1 0', minHeight: 0, overflowY: 'auto' }, rows: { display: 'flex', flexDirection: 'column', gap: s.lg },
  row: { padding: s.lg, minHeight: g.resourceCard, borderBottomWidth: g.hairline, borderBottomStyle: 'solid', borderBottomColor: border.border, fontSize: t.bodySize, lineHeight: t.bodyLeading, whiteSpace: 'pre-wrap' },
  image: { display: 'block', width: '100%', maxWidth: 640, height: 80, objectFit: 'cover', marginBlock: s.md }, expandedImage: { height: 360 },
})
