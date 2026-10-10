import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { expect, userEvent, waitFor, within } from 'storybook/test'
import type { AppendMessage } from '@assistant-ui/react'
import { Button } from 'react-aria-components'
import { Transcript, type TranscriptTurn } from './assistant-ui/composition/Transcript'
import { EmbraceComposer } from './assistant-ui/EmbraceComposer'
import { EmbraceRuntimeProvider } from './assistant-ui/EmbraceRuntime'
import type { TextItem } from './assistant-ui/embrace-data/model'
import { workLogTurnFromItems } from './assistant-ui/taste/work-log'
import { baselineTheme } from './assistant-ui/neutral-theme'
import { lightTheme, type Scheme } from './assistant-ui/composition-theme'
import { accentVars as accent, borderVars as border, geometryVars as g, radiusVars as r, surfaceVars as surface, textVars as ink, spaceVars as s, typeVars as t } from './assistant-ui/composition-tokens.stylex'

const now = Date.parse('2026-01-15T12:30:00Z')
const minute = (index: number) => new Date(Date.parse('2026-01-15T12:00:00Z') + index * 60_000).toISOString()
const prompts = ['Summarize the failing selection check.', 'Keep visible rows grouped after filtering.', 'Explain why the empty state flickers.', 'Check the count after a row is hidden.']
const answers = ['The selection check compares the projected row with the stored selection and reports the first mismatch.', 'Visible rows now stay grouped: the filter runs before grouping, so hidden rows never split a group.', 'The empty state rendered before the first snapshot arrived. It now waits for the initial snapshot.', 'The count follows the visible rows, so hiding a row lowers it by one and selection stays on the same row.']

function turn(index: number, prompt: string | undefined, answer: string | undefined, pending = false): TranscriptTurn {
  const id = pending ? `pending/${index}` : `turn/${index}`
  const user: (TextItem & { role: 'user' }) | undefined = prompt === undefined ? undefined : { _tag: 'Text', id: `${id}/prompt`, role: 'user', text: prompt, attachments: [], streaming: false, at: minute(index), sender: { kind: 'human', label: 'Operator' }, sendState: pending ? { _tag: 'Pending' } : undefined }
  const reply: TextItem | undefined = answer === undefined ? undefined : { _tag: 'Text', id: `${id}/answer`, role: 'assistant', text: answer, attachments: [], streaming: false, at: minute(index), sender: { kind: 'agent', label: 'Assistant' } }
  const items = reply === undefined ? [] : [reply]
  return { id, prompt: user, items, work: workLogTurnFromItems(user === undefined ? items : [user, ...items], { kindFor: () => 'read', running: false, failed: false, interrupted: false, startedAt: minute(index), completeHistory: true }) }
}
const historyTurns = Array.from({ length: 30 }, (_, index) => turn(index, prompts[index % prompts.length], answers[index % answers.length]))
function Surface({ scheme, children }: { scheme: Scheme; children: React.ReactNode }) {
  return <main data-scheme={scheme} {...stylex.props(styles.root, ...baselineTheme, scheme === 'light' && lightTheme)}>{children}</main>
}
const columnTurns = historyTurns.slice(0, 12)
const columnMessages = columnTurns.flatMap(entry => entry.prompt === undefined ? entry.items : [entry.prompt, ...entry.items])
const columnSync = { _tag: 'Live', since: now - 5000 } as const
function ColumnStory({ scheme, readingColumn }: { scheme: Scheme; readingColumn: boolean }) {
  const options = React.useMemo(() => ({ messages: columnMessages, isRunning: false, onNew: async () => {} }), [])
  return <Surface scheme={scheme}><EmbraceRuntimeProvider options={options}>
    <div {...stylex.props(styles.transcript)}><Transcript title="Row projection" turns={columnTurns} sync={columnSync} now={now} observedAt={now - 8000} /></div>
    <div data-testid="story-composer-dock" {...stylex.props(styles.dock)}><EmbraceComposer variant="C1" readingColumn={readingColumn} /></div>
  </EmbraceRuntimeProvider></Surface>
}

const meta = { title: 'Fractal UI/Transcript composer', component: ColumnStory, parameters: { layout: 'fullscreen' }, args: { scheme: 'dark', readingColumn: true }, argTypes: { scheme: { options: ['dark', 'light'], control: 'radio' } } } satisfies Meta<typeof ColumnStory>
export default meta
type Story = StoryObj<typeof meta>

const bounds = (element: Element) => { const rect = element.getBoundingClientRect(); return { left: rect.left, right: rect.right, width: rect.width } }
async function columns(canvasElement: HTMLElement) {
  await document.fonts.ready
  const row = await waitFor(() => { const found = canvasElement.querySelector('[data-testid="transcript-turn"]'); if (found === null) throw new Error('No transcript row'); return found })
  const composer = canvasElement.querySelector('[data-testid="kit-composer"] form')!
  const dock = canvasElement.querySelector('[data-testid="story-composer-dock"]')!
  const lane = parseFloat(getComputedStyle(canvasElement.querySelector('[data-testid="kit-composer"]')!).maxWidth)
  return { row: bounds(row), composer: bounds(composer), dock: bounds(dock), lane }
}

/** The composer and the transcript rows share one column: equal edges, capped at the lane maximum. */
export const ReadingColumn: Story = { play: async ({ canvasElement }) => {
  const { row, composer, lane } = await columns(canvasElement)
  await expect(Math.abs(composer.left - row.left), `composer left ${composer.left} vs transcript left ${row.left}`).toBeLessThanOrEqual(0.5)
  await expect(Math.abs(composer.right - row.right), `composer right ${composer.right} vs transcript right ${row.right}`).toBeLessThanOrEqual(0.5)
  await expect(composer.width).toBeLessThanOrEqual(lane + 0.5)
  await expect(document.documentElement.scrollWidth).toBeLessThanOrEqual(window.innerWidth)
} }
export const ReadingColumnLight: Story = { ...ReadingColumn, args: { scheme: 'light' } }
/** Without the opt-in, the composer keeps filling its host. */
export const HostWidth: Story = { args: { readingColumn: false }, play: async ({ canvasElement }) => {
  const { composer, dock } = await columns(canvasElement)
  await expect(Math.abs(composer.width - dock.width)).toBeLessThanOrEqual(0.5)
} }

const placeholderOptions = { messages: [], isRunning: false, onNew: async () => {} }
const overridePlaceholder = 'Ask about this conversation'
function PlaceholderStory({ scheme }: { scheme: Scheme }) {
  return <Surface scheme={scheme}><div {...stylex.props(styles.placeholders)}>
    {[undefined, overridePlaceholder].map(placeholder => <section key={placeholder ?? 'default'} data-testid={placeholder === undefined ? 'default-placeholder' : 'override-placeholder'}><EmbraceRuntimeProvider options={placeholderOptions}><EmbraceComposer variant="C1" placeholder={placeholder} /></EmbraceRuntimeProvider></section>)}
  </div></Surface>
}
const keyboardText = /enter|shift|return|ctrl|cmd|⌘|⏎|↵|mod\s*\+/i
/** Empty-field guidance never carries keyboard instructions; a host override replaces it, still describing the field. */
export const Placeholder: Story = { render: args => <PlaceholderStory scheme={args.scheme} />, play: async ({ canvasElement }) => {
  for (const [testId, expected] of [['default-placeholder', undefined], ['override-placeholder', overridePlaceholder]] as const) {
    const section = canvasElement.querySelector(`[data-testid="${testId}"]`)!
    const guidance = await waitFor(() => { const found = section.querySelector<HTMLElement>('[data-testid="composer-placeholder"]'); if (found === null) throw new Error(`No guidance in ${testId}`); return found })
    const input = section.querySelector('textarea')!
    await expect(guidance).toBeVisible()
    await expect(guidance.textContent, `${testId} guidance`).not.toMatch(keyboardText)
    await expect(input.placeholder).toBe('')
    await expect(input.getAttribute('aria-describedby')?.split(' ')).toContain(guidance.id)
    if (expected !== undefined) await expect(guidance).toHaveTextContent(expected)
  }
} }
export const PlaceholderLight: Story = { ...Placeholder, args: { scheme: 'light' } }

function OwnSendStory({ scheme }: { scheme: Scheme }) {
  const [turns, setTurns] = React.useState(historyTurns)
  // A defined key from the first render: rerenders under it must never count as a command.
  const [sendKey, setSendKey] = React.useState('pending/opened')
  const next = React.useRef(historyTurns.length)
  const messages = React.useMemo(() => turns.flatMap(entry => entry.prompt === undefined ? entry.items : [entry.prompt, ...entry.items]), [turns])
  const onNew = React.useCallback(async (message: AppendMessage) => {
    const index = next.current++
    const text = message.content.flatMap(part => part.type === 'text' ? [part.text] : []).join('\n')
    setTurns(previous => [...previous, turn(index, text, undefined, true)])
    setSendKey(`pending/${index}`)
  }, [])
  const appendReply = React.useCallback(() => {
    const index = next.current++
    setTurns(previous => [...previous, turn(index, undefined, answers[index % answers.length])])
  }, [])
  const options = React.useMemo(() => ({ messages, isRunning: false, onNew }), [messages, onNew])
  return <Surface scheme={scheme}><EmbraceRuntimeProvider options={options}>
    <div {...stylex.props(styles.toolbar)}><Button onPress={appendReply} {...stylex.props(styles.button)}>Append agent reply</Button></div>
    <div {...stylex.props(styles.transcript)}><Transcript title="Row projection" turns={turns} sync={columnSync} now={now} observedAt={now - 8000} scrollToBottomKey={sendKey} /></div>
    <div {...stylex.props(styles.dock)}><EmbraceComposer variant="C1" readingColumn /></div>
  </EmbraceRuntimeProvider></Surface>
}
async function settleFrames() {
  // The package lib is ES2022 (no Promise.withResolvers). Three frames cover the viewport's scheduled write and capture.
  for (let frame = 0; frame < 3; frame++) await new Promise(resolve => requestAnimationFrame(resolve))
}
/** An agent reply keeps a scrolled-up reader's line and offers the jump; the reader's own send brings it into view. */
export const OwnSendFollows: Story = { render: args => <OwnSendStory scheme={args.scheme} />, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  const viewport = await waitFor(() => { const found = canvasElement.querySelector<HTMLElement>('[data-testid="transcript-scroll"]'); if (found === null || found.querySelectorAll('[data-testid="transcript-turn"]').length < historyTurns.length) throw new Error('History not committed'); return found })
  const jump = canvas.getByText('Scroll to end')
  await waitFor(() => expect(viewport.scrollHeight - viewport.clientHeight - viewport.scrollTop).toBeLessThanOrEqual(2))
  // The reader scrolls up into history.
  viewport.dispatchEvent(new WheelEvent('wheel', { deltaY: -viewport.scrollHeight }))
  viewport.scrollTop = 0
  await settleFrames()
  // Negative control: an incoming reply under the same key never moves the reader.
  await userEvent.click(canvas.getByRole('button', { name: 'Append agent reply' }))
  await waitFor(() => expect(jump).toBeVisible())
  await settleFrames()
  await expect(viewport.scrollTop, 'unchanged key scrolled the reader').toBeLessThanOrEqual(1)
  // The reader's own send changes the key: once its pending row commits, it is in view and the jump is gone.
  await userEvent.type(canvas.getByRole('textbox', { name: 'Message' }), 'Ship the grouped rows{Enter}')
  await waitFor(() => {
    const row = viewport.querySelector('[data-testid="transcript-turn"][data-item-id^="pending/"]')
    if (row === null) throw new Error('Pending send not committed')
    const shown = viewport.getBoundingClientRect(), placed = row.getBoundingClientRect()
    expect(placed.top).toBeGreaterThanOrEqual(shown.top - 0.5)
    expect(placed.bottom).toBeLessThanOrEqual(shown.bottom + 0.5)
    expect(viewport.scrollHeight - viewport.clientHeight - viewport.scrollTop).toBeLessThanOrEqual(2)
    expect(jump).not.toBeVisible()
  }, { timeout: 3000 })
} }
export const OwnSendFollowsLight: Story = { ...OwnSendFollows, args: { scheme: 'light' } }

const styles = stylex.create({
  root: { height: '100vh', width: '100%', boxSizing: 'border-box', display: 'flex', flexDirection: 'column', backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans },
  transcript: { flex: '1 1 0', minHeight: 0, display: 'flex', flexDirection: 'column' },
  dock: { flexShrink: 0, paddingBlock: s.lg },
  placeholders: { display: 'flex', flexDirection: 'column', gap: s.lg, padding: s.lg },
  toolbar: { display: 'flex', gap: s.md, padding: s.md, flexShrink: 0 },
  button: { minHeight: g.controlMd, paddingInline: s.md, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.sm, backgroundColor: surface.controlFill, color: ink.fg, fontFamily: t.fontSans, fontSize: t.metaSize, cursor: 'pointer', ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary } },
})
