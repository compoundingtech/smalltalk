import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { expect, userEvent, waitFor, within } from 'storybook/test'
import { Button } from 'react-aria-components'
import { Transcript, type TranscriptTurn } from './assistant-ui/composition/Transcript'
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

function turn(index: number, prompt: string | undefined, answer: string | undefined, failed = false): TranscriptTurn {
  const id = `turn/${index}`
  const user: (TextItem & { role: 'user' }) | undefined = prompt === undefined ? undefined : { _tag: 'Text', id: `${id}/prompt`, role: 'user', text: prompt, attachments: [], streaming: false, at: minute(index), sender: { kind: 'human', label: 'Operator' }, sendState: failed ? { _tag: 'Failed', reason: { _tag: 'Failed' }, detail: 'The send request timed out.' } : undefined }
  const reply: TextItem | undefined = answer === undefined ? undefined : { _tag: 'Text', id: `${id}/answer`, role: 'assistant', text: answer, attachments: [], streaming: false, at: minute(index), sender: { kind: 'agent', label: 'Assistant' } }
  const items = reply === undefined ? [] : [reply]
  return { id, prompt: user, items, work: workLogTurnFromItems(user === undefined ? items : [user, ...items], { kindFor: () => 'read', running: false, failed: false, interrupted: false, startedAt: minute(index), completeHistory: true }) }
}
const initialTurns = [...Array.from({ length: 24 }, (_, index) => turn(index, prompts[index % prompts.length], answers[index % answers.length])), turn(24, 'Ship the grouped rows.', undefined, true)]
const sync = { _tag: 'Live', since: now - 5000 } as const

function PressStory({ scheme }: { scheme: Scheme }) {
  const [turns, setTurns] = React.useState(initialTurns)
  const [retries, setRetries] = React.useState(0)
  const next = React.useRef(initialTurns.length)
  const messages = React.useMemo(() => turns.flatMap(entry => entry.prompt === undefined ? entry.items : [entry.prompt, ...entry.items]), [turns])
  const options = React.useMemo(() => ({ messages, isRunning: false, onNew: async () => {} }), [messages])
  // A host republishing its snapshot decodes fresh objects for the same rows.
  const republish = React.useCallback(() => setTurns(previous => structuredClone(previous)), [])
  const appendReply = React.useCallback(() => {
    const index = next.current++
    setTurns(previous => [...previous, turn(index, undefined, answers[index % answers.length])])
  }, [])
  const retry = React.useCallback(() => setRetries(count => count + 1), [])
  return <main data-scheme={scheme} {...stylex.props(styles.root, ...baselineTheme, scheme === 'light' && lightTheme)}><EmbraceRuntimeProvider options={options}>
    <div {...stylex.props(styles.toolbar)}><Button onPress={republish} {...stylex.props(styles.button)}>Republish snapshot</Button><Button onPress={appendReply} {...stylex.props(styles.button)}>Append agent reply</Button><span>Retries <output data-testid="retry-count">{retries}</output></span></div>
    <div {...stylex.props(styles.transcript)}><Transcript title="Row projection" turns={turns} sync={sync} now={now} observedAt={now - 8000} onRetrySend={retry} /></div>
  </EmbraceRuntimeProvider></main>
}

const meta = { title: 'Fractal UI/Transcript press', component: PressStory, parameters: { layout: 'fullscreen' }, args: { scheme: 'dark' }, argTypes: { scheme: { options: ['dark', 'light'], control: 'radio' } } } satisfies Meta<typeof PressStory>
export default meta
type Story = StoryObj<typeof meta>

async function settleFrames() {
  // The package lib is ES2022 (no Promise.withResolvers). Three frames cover the viewport's scheduled write and capture.
  for (let frame = 0; frame < 3; frame++) await new Promise(resolve => requestAnimationFrame(resolve))
}
async function ready(canvasElement: HTMLElement) {
  await document.fonts.ready
  const viewport = await waitFor(() => { const found = canvasElement.querySelector<HTMLElement>('[data-testid="transcript-scroll"]'); if (found === null || found.querySelectorAll('[data-testid="transcript-turn"]').length < initialTurns.length) throw new Error('History not committed'); return found })
  await waitFor(() => expect(viewport.scrollHeight - viewport.clientHeight - viewport.scrollTop).toBeLessThanOrEqual(2))
  const canvas = within(canvasElement)
  return { viewport, canvas, jump: canvas.getByText('New messages ↓'), retries: canvas.getByTestId('retry-count') }
}
/** A mouse press by coordinates: the release lands on whatever is under the pointer after `during`, and the click goes to the common ancestor, as in a browser. */
async function pressAcross(target: HTMLElement, during: () => Promise<void>) {
  const rect = target.getBoundingClientRect()
  const init = { bubbles: true, cancelable: true, composed: true, clientX: rect.left + rect.width / 2, clientY: rect.top + rect.height / 2, button: 0, pointerId: 1, pointerType: 'mouse', isPrimary: true, width: 1, height: 1 }
  target.dispatchEvent(new PointerEvent('pointerdown', { ...init, buttons: 1, pressure: 0.5 }))
  target.dispatchEvent(new MouseEvent('mousedown', { ...init, buttons: 1, detail: 1 }))
  await during()
  const landed = document.elementFromPoint(init.clientX, init.clientY)!
  landed.dispatchEvent(new PointerEvent('pointerup', { ...init, buttons: 0, pressure: 0 }))
  landed.dispatchEvent(new MouseEvent('mouseup', { ...init, buttons: 0, detail: 1 }))
  let common: Element | null = landed
  while (common !== null && !common.contains(target)) common = common.parentElement
  common?.dispatchEvent(new MouseEvent('click', { ...init, buttons: 0, detail: 1 }))
}

/** A republished snapshot mid-press neither reveals the jump nor steals the Retry click. */
export const SameRowsDuringPress: Story = { play: async ({ canvasElement }) => {
  const { canvas, jump, retries } = await ready(canvasElement)
  await pressAcross(canvas.getByRole('button', { name: 'Retry' }), async () => {
    canvas.getByRole('button', { name: 'Republish snapshot' }).click()
    await settleFrames()
    await expect(jump, 'jump revealed during the press').not.toBeVisible()
  })
  await waitFor(() => expect(retries).toHaveTextContent('1'))
  await settleFrames()
  await expect(retries).toHaveTextContent('1')
  await expect(jump, 'same rows marked unread').not.toBeVisible()
} }
export const SameRowsDuringPressLight: Story = { ...SameRowsDuringPress, args: { scheme: 'light' } }

/** A genuinely new row during the press still lets Retry take the click; the jump appears only after release. */
export const NewRowDuringPress: Story = { play: async ({ canvasElement }) => {
  const { canvas, jump, retries } = await ready(canvasElement)
  await pressAcross(canvas.getByRole('button', { name: 'Retry' }), async () => {
    canvas.getByRole('button', { name: 'Append agent reply' }).click()
    await settleFrames()
    await expect(jump, 'jump revealed during the press').not.toBeVisible()
  })
  await waitFor(() => expect(retries).toHaveTextContent('1'))
  await waitFor(() => expect(jump).toBeVisible())
  await expect(retries).toHaveTextContent('1')
} }
export const NewRowDuringPressLight: Story = { ...NewRowDuringPress, args: { scheme: 'light' } }

/** A scrolled-up reader is not told about rows that were already there. */
export const SameRowsScrolledUp: Story = { play: async ({ canvasElement }) => {
  const { viewport, canvas, jump } = await ready(canvasElement)
  viewport.dispatchEvent(new WheelEvent('wheel', { deltaY: -viewport.scrollHeight }))
  viewport.scrollTop = 0
  await settleFrames()
  await userEvent.click(canvas.getByRole('button', { name: 'Republish snapshot' }))
  await settleFrames()
  await expect(jump, 'same rows marked unread').not.toBeVisible()
  // Positive control: a new row still marks unread.
  await userEvent.click(canvas.getByRole('button', { name: 'Append agent reply' }))
  await waitFor(() => expect(jump).toBeVisible())
} }
export const SameRowsScrolledUpLight: Story = { ...SameRowsScrolledUp, args: { scheme: 'light' } }

const styles = stylex.create({
  root: { height: '100vh', width: '100%', boxSizing: 'border-box', display: 'flex', flexDirection: 'column', backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans },
  transcript: { flex: '1 1 0', minHeight: 0, display: 'flex', flexDirection: 'column' },
  toolbar: { display: 'flex', alignItems: 'center', gap: s.md, padding: s.md, flexShrink: 0, fontSize: t.metaSize },
  button: { minHeight: g.controlMd, paddingInline: s.md, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.sm, backgroundColor: surface.controlFill, color: ink.fg, fontFamily: t.fontSans, fontSize: t.metaSize, cursor: 'pointer', ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary } },
})
