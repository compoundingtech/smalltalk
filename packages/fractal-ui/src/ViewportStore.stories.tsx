import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { expect, userEvent, waitFor, within } from 'storybook/test'
import { Button } from 'react-aria-components'
import { EmbraceScrollViewport, ViewportStore, ViewportStoreContext, type ViewportState } from './assistant-ui/EmbraceScrollViewport'
import { EmbraceVirtualConversation } from './assistant-ui/EmbraceVirtualConversation'
import { baselineTheme } from './assistant-ui/neutral-theme'
import { lightTheme, type Scheme } from './assistant-ui/composition-theme'
import { surfaceVars as surface, textVars as ink, borderVars as border, accentVars as accent, spaceVars as s, typeVars as t, geometryVars as g } from './assistant-ui/composition-tokens.stylex'

// This package targets ES2022; Promise.withResolvers is not part of its declared library.
const frame = () => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve())))
function StoreStory({ scheme = 'dark', virtual = false }: { scheme?: Scheme; virtual?: boolean }) {
  const [store] = React.useState(() => new ViewportStore())
  const [mount, setMount] = React.useState(0)
  const [command, setCommand] = React.useState(0)
  const [notifications, setNotifications] = React.useState(0)
  const [captured, setCaptured] = React.useState<ReadonlyArray<{ key: string; state: ViewportState }>>([])
  const rows = React.useMemo(() => Array.from({ length: 80 }, (_, index) => ({ id: `entry-${index}`, text: `Reading entry ${index + 1}: browser-local positions remain stable while another tab shares a newer position.` })), [])
  React.useLayoutEffect(() => store.subscribe(() => setNotifications(value => value + 1)), [store])
  const renderRow = (row: typeof rows[number]) => <article data-item-id={row.id} {...stylex.props(styles.row)}><p data-follow-anchor {...stylex.props(styles.paragraph)}>{row.text}</p></article>
  return <ViewportStoreContext.Provider value={store}><main {...stylex.props(styles.root, ...baselineTheme, scheme === 'light' && lightTheme)}>
    <div {...stylex.props(styles.toolbar)}>
      <Button {...stylex.props(styles.button)} onPress={() => setCaptured(store.snapshot())}>Capture active position</Button>
      <Button {...stylex.props(styles.button)} onPress={() => {
        const other = new ViewportStore()
        other.save('one', { top: 960, following: false, unread: true })
        store.hydrate(other.snapshot())
      }}>Hydrate newer position</Button>
      <Button {...stylex.props(styles.button)} onPress={() => setMount(value => value + 1)}>Remount conversation</Button>
      <Button {...stylex.props(styles.button)} onPress={() => setCommand(value => value + 1)}>Send own message</Button>
      <span data-testid="notifications">{notifications}</span>
      <span data-testid="captured-top">{captured[0]?.state.top ?? 'No position captured'}</span>
      <span data-testid="captured-follow">{captured[0]?.state.following === undefined ? '' : String(captured[0].state.following)}</span>
    </div>
    {virtual ? <EmbraceVirtualConversation key={mount} items={rows} renderItem={renderRow} anchorKey="one" scrollToBottomKey={String(command)} /> : <EmbraceScrollViewport key={mount} items={rows} stateKey="one" scrollToBottomKey={String(command)} data-testid="transcript-scroll" aria-label="Conversation history" {...stylex.props(styles.viewport)} contentProps={stylex.props(styles.rows)}>{rows.map(row => <React.Fragment key={row.id}>{renderRow(row)}</React.Fragment>)}</EmbraceScrollViewport>}
  </main></ViewportStoreContext.Provider>
}
const meta = { title: 'Fractal UI/Viewport memory', component: StoreStory, args: { scheme: 'dark', virtual: false }, parameters: { layout: 'fullscreen' }, argTypes: { scheme: { options: ['dark', 'light'], control: 'radio' }, virtual: { control: 'boolean' } } } satisfies Meta<typeof StoreStory>
export default meta
type Story = StoryObj<typeof meta>
async function ready(canvasElement: HTMLElement) {
  const canvas = within(canvasElement)
  const viewport = canvas.getByTestId('transcript-scroll')
  await waitFor(() => expect(viewport.scrollHeight - viewport.clientHeight - viewport.scrollTop).toBeLessThanOrEqual(2))
  return { canvas, viewport }
}
async function scrollByReader(viewport: HTMLElement, top: number) {
  viewport.dispatchEvent(new WheelEvent('wheel', { deltaY: top - viewport.scrollTop }))
  viewport.scrollTop = top
  viewport.dispatchEvent(new Event('scroll'))
  await frame()
}
export const SnapshotHydrateRoundTrip: Story = { play: async () => {
  const source = new ViewportStore()
  source.save('one', { top: 420, following: false, unread: true, anchor: { rowId: 'entry-7', text: 'Reading entry 8', offset: -10 } })
  source.save('two', { top: 800, following: true, unread: false })
  const entries = source.snapshot()
  await expect(entries[0]!.state.updatedAt).toBeGreaterThan(0)
  await expect(entries[1]!.state.updatedAt).toBeGreaterThan(entries[0]!.state.updatedAt)
  const serialized: ReadonlyArray<{ key: string; state: ViewportState }> = JSON.parse(JSON.stringify(entries))
  const destination = new ViewportStore()
  destination.hydrate(serialized)
  await expect(destination.snapshot()).toEqual(entries)
} }
export const OlderEntryCannotOverwrite: Story = { play: async () => {
  const older = new ViewportStore()
  older.save('one', { top: 100, following: false, unread: true })
  const newer = new ViewportStore()
  newer.save('one', { top: 840, following: false, unread: true })
  const destination = new ViewportStore()
  destination.hydrate(newer.snapshot())
  destination.hydrate(older.snapshot())
  await expect(destination.snapshot()).toEqual(newer.snapshot())
} }
export const SaveRetainSubscribe: Story = { play: async () => {
  const store = new ViewportStore()
  let notifications = 0
  const unsubscribe = store.subscribe(() => notifications++)
  store.save('one', { top: 100, following: false, unread: true })
  store.save('two', { top: 840, following: false, unread: true })
  await expect(notifications).toBe(2)
  store.retain(new Set(['one']))
  await expect(notifications).toBe(3)
  await expect(store.snapshot().map(entry => entry.key)).toEqual(['one'])
  unsubscribe()
  store.save('one', { top: 200, following: true, unread: false })
  await expect(notifications).toBe(3)
} }
export const LiveScrollPublishesThrottled: Story = { play: async ({ canvasElement }) => {
  const { canvas, viewport } = await ready(canvasElement)
  const before = Number(canvas.getByTestId('notifications').textContent)
  await scrollByReader(viewport, 240)
  await scrollByReader(viewport, 340)
  await scrollByReader(viewport, 440)
  const early = Number(canvas.getByTestId('notifications').textContent) - before
  await expect(early).toBeGreaterThanOrEqual(1)
  await expect(early).toBeLessThanOrEqual(2)
  await userEvent.click(canvas.getByRole('button', { name: 'Capture active position' }))
  await expect(Number(canvas.getByTestId('captured-top').textContent)).toBe(viewport.scrollTop)
  await waitFor(() => expect(Number(canvas.getByTestId('notifications').textContent)).toBeGreaterThan(before + 1))
  const afterScroll = Number(canvas.getByTestId('notifications').textContent)
  await userEvent.click(canvas.getByRole('button', { name: 'Send own message' }))
  await waitFor(() => expect(Number(canvas.getByTestId('notifications').textContent)).toBeGreaterThan(afterScroll))
  await waitFor(() => expect(viewport.scrollHeight - viewport.clientHeight - viewport.scrollTop).toBeLessThanOrEqual(2))
  await userEvent.click(canvas.getByRole('button', { name: 'Capture active position' }))
  await expect(canvas.getByTestId('captured-follow')).toHaveTextContent('true')
} }
export const HydrateMountedAppliesOnRemount: Story = { play: async ({ canvasElement }) => {
  const { canvas, viewport } = await ready(canvasElement)
  await scrollByReader(viewport, 240)
  const top = viewport.scrollTop
  await userEvent.click(canvas.getByRole('button', { name: 'Hydrate newer position' }))
  // Let the local trailing publish settle: it must not overwrite the newer remote entry.
  for (let sample = 0; sample < 10; sample++) await frame()
  await expect(viewport.scrollTop).toBe(top)
  await userEvent.click(canvas.getByRole('button', { name: 'Capture active position' }))
  await expect(Number(canvas.getByTestId('captured-top').textContent)).toBe(960)
  await userEvent.click(canvas.getByRole('button', { name: 'Remount conversation' }))
  const remounted = canvas.getByTestId('transcript-scroll')
  await waitFor(() => expect(Math.abs(remounted.scrollTop - 960)).toBeLessThanOrEqual(2))
  await waitFor(() => expect(canvas.getByRole('button', { name: 'Scroll to end' })).toBeVisible())
} }
export const AllStates: Story = { render: args => <StoreStory {...args} /> }
export const SnapshotHydrateRoundTripLight: Story = { ...SnapshotHydrateRoundTrip, args: { scheme: 'light' } }
export const OlderEntryCannotOverwriteLight: Story = { ...OlderEntryCannotOverwrite, args: { scheme: 'light' } }
export const SaveRetainSubscribeLight: Story = { ...SaveRetainSubscribe, args: { scheme: 'light' } }
export const LiveScrollPublishesThrottledLight: Story = { ...LiveScrollPublishesThrottled, args: { scheme: 'light' } }
export const HydrateMountedAppliesOnRemountLight: Story = { ...HydrateMountedAppliesOnRemount, args: { scheme: 'light' } }
export const VirtualLiveScrollPublishesThrottled: Story = { ...LiveScrollPublishesThrottled, args: { virtual: true } }
export const VirtualHydrateMountedAppliesOnRemount: Story = { ...HydrateMountedAppliesOnRemount, args: { virtual: true } }
const styles = stylex.create({
  root: { display: 'flex', flexDirection: 'column', height: '100vh', boxSizing: 'border-box', padding: s.lg, gap: s.md, backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans },
  toolbar: { display: 'flex', flexWrap: 'wrap', alignItems: 'center', gap: s.md, flexShrink: 0 },
  button: { minHeight: g.controlMd, paddingInline: s.md, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, backgroundColor: surface.controlFill, color: ink.fg, fontSize: t.metaSize, cursor: 'pointer', ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary } },
  viewport: { flex: '1 1 0', minHeight: 0, overflowY: 'auto' }, rows: { display: 'flex', flexDirection: 'column', gap: s.lg },
  row: { padding: s.lg, minHeight: g.resourceCard, borderBottomWidth: g.hairline, borderBottomStyle: 'solid', borderBottomColor: border.border, fontSize: t.bodySize, lineHeight: t.bodyLeading }, paragraph: { margin: 0 },
})
