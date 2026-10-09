import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { expect, userEvent, within } from 'storybook/test'
import { Button } from 'react-aria-components'
import { WorkLogV1 } from './assistant-ui/taste/WorkLogV1'
import { ThinkingEntry } from './assistant-ui/composition/ThinkingEntry'
import { Transcript } from './assistant-ui/composition/Transcript'
import { EmbraceRuntimeProvider } from './assistant-ui/EmbraceRuntime'
import { Input, CommandMenu } from './kit'
import { EmbraceComposer, type EmbraceComposerHandle } from './assistant-ui/EmbraceComposer'
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
const styles = stylex.create({ root: { padding: s.xl, backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans, minHeight: '100vh' } })
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
