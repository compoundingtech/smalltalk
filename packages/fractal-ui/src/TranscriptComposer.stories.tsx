import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { expect, waitFor } from 'storybook/test'
import { Transcript, type TranscriptTurn } from './assistant-ui/composition/Transcript'
import { EmbraceComposer } from './assistant-ui/EmbraceComposer'
import { EmbraceRuntimeProvider } from './assistant-ui/EmbraceRuntime'
import type { TextItem } from './assistant-ui/embrace-data/model'
import { workLogTurnFromItems } from './assistant-ui/taste/work-log'
import { baselineTheme } from './assistant-ui/neutral-theme'
import { lightTheme, type Scheme } from './assistant-ui/composition-theme'
import { surfaceVars as surface, textVars as ink, spaceVars as s, typeVars as t } from './assistant-ui/composition-tokens.stylex'

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

const styles = stylex.create({
  root: { height: '100vh', width: '100%', boxSizing: 'border-box', display: 'flex', flexDirection: 'column', backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans },
  transcript: { flex: '1 1 0', minHeight: 0, display: 'flex', flexDirection: 'column' },
  dock: { flexShrink: 0, paddingBlock: s.lg },
})
