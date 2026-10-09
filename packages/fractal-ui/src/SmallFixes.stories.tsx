import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { expect, userEvent, within, waitFor } from 'storybook/test'
import { Button } from 'react-aria-components'
import { MessageNotSentError } from '@assistant-ui/react'
import { WorkLogV1 } from './assistant-ui/taste/WorkLogV1'
import { ThinkingEntry } from './assistant-ui/composition/ThinkingEntry'
import { Transcript } from './assistant-ui/composition/Transcript'
import { ErrorOverlay } from './assistant-ui/composition/ErrorOverlay'
import { elevationVars as elevation } from './assistant-ui/composition-tokens.stylex'
import { EmbraceRuntimeProvider } from './assistant-ui/EmbraceRuntime'
import { Input, CommandMenu } from './kit'
import { EmbraceComposer, type EmbraceComposerHandle, type ComposerDraftCause } from './assistant-ui/EmbraceComposer'
import { baselineTheme } from './assistant-ui/neutral-theme'
import { lightTheme, type Scheme } from './assistant-ui/composition-theme'
import { surfaceVars as surface, textVars as ink, spaceVars as s, typeVars as t } from './assistant-ui/composition-tokens.stylex'

function WorkFocus({ scheme = 'dark' }: { scheme?: Scheme }) {
  return <section data-scheme={scheme} {...stylex.props(styles.root, ...baselineTheme, scheme === 'light' && lightTheme)}>
    <Button>Before work log</Button>
    <WorkLogV1 turn={{ calls: [], durationMs: undefined, running: true, failed: false, interrupted: false }} hideLiveRow expandedBody={Array.from({ length: 10 }, (_, index) => <ThinkingEntry key={index} text={`Check synthetic step ${index + 1} before proceeding.`} />)} />
    <Button>After work log</Button>
  </section>
}
const styles = stylex.create({
  root: { padding: s.xl, backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans, minHeight: '100vh' },
  dialogElevation: { marginTop: s.xl, padding: s.xl, backgroundColor: surface.raised, boxShadow: elevation.dialog },
  dock: { position: 'fixed', bottom: s.xl, insetInline: s.xl },
})
const meta = { title: 'Fractal UI/Small Fixes', component: WorkFocus, args: { scheme: 'dark' }, parameters: { layout: 'fullscreen' } } satisfies Meta<typeof WorkFocus>
export default meta
type Story = StoryObj<typeof meta>
export const WorkLogRovingFocus: Story = { play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  const before = canvas.getByRole('button', { name: 'Before work log' })
  const after = canvas.getByRole('button', { name: 'After work log' })
  const rows = canvas.getAllByRole('button', { name: 'Thinking' })
  before.focus()
  await userEvent.tab()
  await expect(rows[0]).toHaveFocus()
  await userEvent.keyboard('{ArrowDown}')
  await expect(rows[1]).toHaveFocus()
  await userEvent.keyboard('{Enter}')
  await expect(rows[1]).toHaveAttribute('aria-expanded', 'true')
  await userEvent.keyboard('{ArrowUp}')
  await expect(rows[0]).toHaveFocus()
  await userEvent.tab()
  await expect(after).toHaveFocus()
  await userEvent.tab({ shift: true })
  await expect(rows[0]).toHaveFocus()
  await userEvent.tab({ shift: true })
  await expect(before).toHaveFocus()
} }
export const WorkLogRovingFocusLight: Story = { ...WorkLogRovingFocus, args: { scheme: 'light' } }

const loadingNow = Date.parse('2026-01-15T12:00:08Z')
function LoadingConversation({ scheme = 'dark' }: { scheme?: Scheme }) {
  return <section {...stylex.props(styles.root, ...baselineTheme, scheme === 'light' && lightTheme)}><EmbraceRuntimeProvider options={{ messages: [], isRunning: false, onNew: async () => {} }}><Transcript title="Synthetic conversation" turns={[]} sync={{ _tag: 'Connecting', attempt: 1, since: loadingNow - 8000 }} now={loadingNow} observedAt={loadingNow - 8000} /></EmbraceRuntimeProvider></section>
}
export const SingleLoadingLabel: Story = { render: args => <LoadingConversation {...args} />, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  const placeholder = await canvas.findByTestId('transcript-placeholder')
  await expect(within(placeholder).getAllByText(/Loading conversation/)).toHaveLength(1)
  await expect(within(placeholder).getAllByRole('status')).toHaveLength(1)
  await expect(placeholder).not.toHaveAttribute('aria-label', 'Loading conversation')
  await expect(within(placeholder).getByTestId('sync-line')).toHaveAttribute('data-sync-visible', 'true')
} }
export const SingleLoadingLabelLight: Story = { ...SingleLoadingLabel, args: { scheme: 'light' } }

function SearchContrast({ scheme = 'dark' }: { scheme?: Scheme }) {
  const [value, setValue] = React.useState('')
  return <section {...stylex.props(styles.root, ...baselineTheme, scheme === 'light' && lightTheme)}><Input label="Search agents" value={value} onChange={setValue} placeholder="Search agents…" /><CommandMenu groups={[]} placeholder="Search commands…" /></section>
}
function contrast(a: string, b: string) {
  const luminance = (value: string) => {
    const channels = value.match(/[\d.]+/g)!.slice(0, 3).map(channel => { const n = Number(channel) / 255; return n <= 0.04045 ? n / 12.92 : ((n + 0.055) / 1.055) ** 2.4 })
    return channels[0]! * 0.2126 + channels[1]! * 0.7152 + channels[2]! * 0.0722
  }
  const x = luminance(a), y = luminance(b)
  return (Math.max(x, y) + 0.05) / (Math.min(x, y) + 0.05)
}
export const SearchFieldContrast: Story = { render: args => <SearchContrast {...args} />, play: async ({ canvasElement }) => {
  const fields = canvasElement.querySelectorAll('input')
  await expect(fields).toHaveLength(2)
  for (const field of fields) {
    const root = field.parentElement!
    const background = getComputedStyle(root).backgroundColor
    await expect(contrast(getComputedStyle(root).borderTopColor, background)).toBeGreaterThanOrEqual(3)
    await expect(contrast(getComputedStyle(field).color, background)).toBeGreaterThanOrEqual(4.5)
    await expect(contrast(getComputedStyle(field, '::placeholder').color, background)).toBeGreaterThanOrEqual(4.5)
  }
} }
export const SearchFieldContrastLight: Story = { ...SearchFieldContrast, args: { scheme: 'light' } }

function DraftRestore({ scheme = 'dark' }: { scheme?: Scheme }) {
  const composer = React.useRef<EmbraceComposerHandle>(null)
  const [result, setResult] = React.useState('')
  const [revision, setRevision] = React.useState(0)
  return <section {...stylex.props(styles.root, ...baselineTheme, scheme === 'light' && lightTheme)}><EmbraceRuntimeProvider options={{ messages: [], isRunning: false, onNew: async () => {} }}>
    <Button onPress={() => setRevision(composer.current!.getDraft().revision)}>Start storage read</Button>
    <Button onPress={() => setResult(composer.current?.restoreDraft({ text: 'Saved browser-local draft', savedAt: 1, expectedRevision: revision }) ? 'Applied' : 'Kept newer draft')}>Finish storage read</Button>
    <EmbraceComposer ref={composer} variant="C1" onDraftChange={draft => setResult(`Revision ${draft.revision}`)} />
    <output data-testid="draft-result">{result}</output>
  </EmbraceRuntimeProvider></section>
}
export const RestoreEmptyDraft: Story = { render: args => <DraftRestore {...args} />, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  await userEvent.click(canvas.getByRole('button', { name: 'Start storage read' }))
  await userEvent.click(canvas.getByRole('button', { name: 'Finish storage read' }))
  await expect(canvas.getByRole('textbox', { name: 'Message' })).toHaveValue('Saved browser-local draft')
  await expect(canvas.getByTestId('draft-result')).toHaveTextContent('Applied')
} }
export const RestoreEmptyDraftLight: Story = { ...RestoreEmptyDraft, args: { scheme: 'light' } }
export const PreserveNewerDraft: Story = { render: args => <DraftRestore {...args} />, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  await userEvent.click(canvas.getByRole('button', { name: 'Start storage read' }))
  const input = canvas.getByRole('textbox', { name: 'Message' })
  await userEvent.type(input, 'Newer user text')
  await userEvent.click(canvas.getByRole('button', { name: 'Finish storage read' }))
  await expect(input).toHaveValue('Newer user text')
  await expect(canvas.getByTestId('draft-result')).toHaveTextContent('Kept newer draft')
  // Returning to empty still counts as a newer edit; stale text must not reappear.
  await userEvent.clear(input)
  await userEvent.click(canvas.getByRole('button', { name: 'Finish storage read' }))
  await expect(input).toHaveValue('')
} }
export const PreserveNewerDraftLight: Story = { ...PreserveNewerDraft, args: { scheme: 'light' } }

function ConnectionDock({ scheme = 'dark' }: { scheme?: Scheme }) {
  const [notice, setNotice] = React.useState<'offline' | 'reconnecting'>()
  const [actions, setActions] = React.useState(0)
  return <section {...stylex.props(styles.root, ...baselineTheme, scheme === 'light' && lightTheme)}><div {...stylex.props(styles.dock)}><EmbraceRuntimeProvider options={{ messages: [], isRunning: false, onNew: async () => {} }}>
    <EmbraceComposer variant="C1" onDraftChange={draft => setNotice(draft.text === '' ? undefined : 'offline')} connectionNotice={notice === undefined ? undefined : { tone: notice, text: notice === 'offline' ? 'Offline: your browser-local draft remains editable.' : 'Reconnecting to the conversation…', action: notice === 'offline' ? { label: 'Reconnect', onPress: () => { setActions(value => value + 1); setNotice('reconnecting') } } : undefined }} />
    <output data-testid="connection-actions">{actions}</output>
  </EmbraceRuntimeProvider></div></section>
}
export const ComposerConnectionNotice: Story = { render: args => <ConnectionDock {...args} />, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  const input = canvas.getByRole('textbox', { name: 'Message' })
  await expect(canvas.queryByTestId('composer-connection-notice')).toBeNull()
  await expect(canvas.getByTestId('kit-composer').getBoundingClientRect().height).toBe(canvasElement.querySelector('form')!.getBoundingClientRect().height)
  input.focus()
  const before = input.getBoundingClientRect()
  let inputShift = 0
  const observer = new PerformanceObserver(list => {
    for (const entry of list.getEntries()) {
      const shift = entry as PerformanceEntry & { value: number; sources?: readonly { node?: Node }[] }
      if (shift.sources?.some(source => source.node === input || source.node instanceof Element && source.node.contains(input))) inputShift += shift.value
    }
  })
  observer.observe({ type: 'layout-shift' })
  try {
    await userEvent.type(input, 'Keep my draft')
    const notice = await canvas.findByTestId('composer-connection-notice')
    await expect(notice).toHaveAttribute('role', 'status')
    await expect(notice).toHaveAttribute('aria-live', 'polite')
    await expect(notice).toHaveTextContent('Offline: your browser-local draft remains editable.')
    await expect(input).toHaveFocus()
    await new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve())))
    const after = input.getBoundingClientRect()
    await expect([after.x, after.y, after.width, after.height]).toEqual([before.x, before.y, before.width, before.height])
    await expect(inputShift).toBe(0)
    await userEvent.tab({ shift: true })
    await expect(canvas.getByRole('button', { name: 'Reconnect' })).toHaveFocus()
    await userEvent.keyboard('{Enter}')
    await expect(canvas.getByTestId('connection-actions')).toHaveTextContent('1')
    await waitFor(() => expect(notice).toHaveTextContent('Reconnecting to the conversation…'))
    await expect(within(notice).queryByRole('button')).toBeNull()
    await expect(input).toHaveValue('Keep my draft')
    await userEvent.clear(input)
    await waitFor(() => expect(canvas.queryByTestId('composer-connection-notice')).toBeNull())
    await new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve())))
    const removed = input.getBoundingClientRect()
    await expect([removed.x, removed.y, removed.width, removed.height]).toEqual([before.x, before.y, before.width, before.height])
    await expect(input).toHaveFocus()
    await expect(inputShift).toBe(0)
    await expect(canvas.getByTestId('kit-composer').getBoundingClientRect().height).toBe(canvasElement.querySelector('form')!.getBoundingClientRect().height)
  } finally { observer.disconnect() }
} }
export const ComposerConnectionNoticeLight: Story = { ...ComposerConnectionNotice, args: { scheme: 'light' } }

function DraftCauses({ scheme = 'dark' }: { scheme?: Scheme }) {
  const reject = React.useRef<((error: Error) => void) | undefined>(undefined)
  const [events, setEvents] = React.useState<readonly { cause: ComposerDraftCause; text: string }[]>([])
  // The kit targets ES2022, whose Promise API requires an executor for externally settled work.
  return <section {...stylex.props(styles.root, ...baselineTheme, scheme === 'light' && lightTheme)}><EmbraceRuntimeProvider options={{ messages: [], isRunning: false, onNew: () => new Promise<void>((_, fail) => { reject.current = fail }) }}>
    <EmbraceComposer variant="C1" onDraftChange={({ cause, text }) => setEvents(previous => [...previous, { cause, text }])} />
    <Button onPress={() => setEvents([])}>Clear event log</Button>
    <Button onPress={() => reject.current?.(new MessageNotSentError())}>Reject pending send</Button>
    <output data-testid="draft-events">{JSON.stringify(events)}</output>
  </EmbraceRuntimeProvider></section>
}
export const DraftChangeCauses: Story = { render: args => <DraftCauses {...args} />, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  const input = canvas.getByRole('textbox', { name: 'Message' })
  await userEvent.type(input, 'Sent text')
  await userEvent.click(canvas.getByRole('button', { name: 'Clear event log' }))
  await userEvent.click(canvas.getByRole('button', { name: 'Send' }))
  await expect(input).toHaveValue('')
  await expect(canvas.getByTestId('draft-events')).toHaveTextContent('[{"cause":"submit-reset","text":""}]')
  await userEvent.click(canvas.getByRole('button', { name: 'Reject pending send' }))
  await waitFor(() => expect(input).toHaveValue('Sent text'))
  await expect(canvas.getByTestId('draft-events')).toHaveTextContent('"cause":"send-failed-restore"')
  await userEvent.click(canvas.getByRole('button', { name: 'Clear event log' }))
  await userEvent.clear(input)
  await expect(canvas.getByTestId('draft-events')).toHaveTextContent('[{"cause":"user","text":""}]')
} }
export const DraftChangeCausesLight: Story = { ...DraftChangeCauses, args: { scheme: 'light' } }
export const FailedSendPreservesNewerDraft: Story = { render: args => <DraftCauses {...args} />, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  const input = canvas.getByRole('textbox', { name: 'Message' })
  await userEvent.type(input, 'Sent text')
  await userEvent.click(canvas.getByRole('button', { name: 'Send' }))
  await expect(input).toHaveValue('')
  await userEvent.type(input, 'Newer text')
  await userEvent.click(canvas.getByRole('button', { name: 'Clear event log' }))
  await userEvent.click(canvas.getByRole('button', { name: 'Reject pending send' }))
  await new Promise<void>(resolve => setTimeout(resolve, 100))
  await expect(input).toHaveValue('Newer text')
  await expect(canvas.getByTestId('draft-events')).toHaveTextContent('[]')
} }
export const FailedSendPreservesNewerDraftLight: Story = { ...FailedSendPreservesNewerDraft, args: { scheme: 'light' } }

function ElevationSurface({ scheme = 'dark' }: { scheme?: Scheme }) {
  return <section {...stylex.props(styles.root, ...baselineTheme, scheme === 'light' && lightTheme)}>
    <ErrorOverlay id="synthetic-elevation-failure" title="Run failed" detail="The synthetic command did not complete." />
    <div data-testid="dialog-elevation" {...stylex.props(styles.dialogElevation)}>Dialog elevation token</div>
  </section>
}
export const LightElevation: Story = { args: { scheme: 'light' }, render: args => <ElevationSurface {...args} />, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  const banner = await canvas.findByRole('alert')
  await expect(getComputedStyle(banner).boxShadow).toBe('rgba(0, 0, 0, 0.18) 0px 18px 44px -18px')
  await expect(getComputedStyle(canvas.getByTestId('dialog-elevation')).boxShadow).toBe('rgba(0, 0, 0, 0.22) 0px 24px 64px -24px')
} }
