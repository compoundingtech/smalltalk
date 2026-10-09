import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { expect, userEvent, waitFor, within } from 'storybook/test'
import { Button } from 'react-aria-components'
import { Transcript, type TranscriptTurn } from './assistant-ui/composition/Transcript'
import { EmbraceRuntimeProvider } from './assistant-ui/EmbraceRuntime'
import { EmbraceScrollViewport } from './assistant-ui/EmbraceScrollViewport'
import type { TextItem, ToolCallItem } from './assistant-ui/embrace-data/model'
import { workLogTurnFromItems } from './assistant-ui/taste/work-log'
import { baselineTheme } from './assistant-ui/neutral-theme'
import { lightTheme, type Scheme } from './assistant-ui/composition-theme'
import { accentVars as accent, borderVars as border, geometryVars as g, radiusVars as r, surfaceVars as surface, textVars as ink, spaceVars as s, typeVars as t } from './assistant-ui/composition-tokens.stylex'

const now = Date.parse('2026-01-15T12:30:00Z')
const minute = (index: number) => new Date(Date.parse('2026-01-15T12:00:00Z') + index * 60_000).toISOString()
const prompts = ['Summarize the failing selection check.', 'Keep visible rows grouped after filtering.', 'Explain why the empty state flickers.', 'Check the count after a row is hidden.']
const answers = ['The selection check compares the projected row with the stored selection and reports the first mismatch.', 'Visible rows now stay grouped: the filter runs before grouping, so hidden rows never split a group.', 'The empty state rendered before the first snapshot arrived. It now waits for the initial snapshot.', 'The count follows the visible rows, so hiding a row lowers it by one and selection stays on the same row.']

function turn(index: number, prompt: string | undefined, answer: string | undefined, { failed = false, tool }: { failed?: boolean; tool?: ToolCallItem } = {}): TranscriptTurn {
  const id = `turn/${index}`
  const user: (TextItem & { role: 'user' }) | undefined = prompt === undefined ? undefined : { _tag: 'Text', id: `${id}/prompt`, role: 'user', text: prompt, attachments: [], streaming: false, at: minute(index), sender: { kind: 'human', label: 'Operator' }, sendState: failed ? { _tag: 'Failed', reason: { _tag: 'Failed' }, detail: 'The send request timed out.' } : undefined }
  const reply: TextItem | undefined = answer === undefined ? undefined : { _tag: 'Text', id: `${id}/answer`, role: 'assistant', text: answer, attachments: [], streaming: false, at: minute(index), sender: { kind: 'agent', label: 'Assistant' } }
  const items = [...(tool === undefined ? [] : [tool]), ...(reply === undefined ? [] : [reply])]
  return { id, prompt: user, items, work: workLogTurnFromItems(user === undefined ? items : [user, ...items], { kindFor: () => 'read', running: false, failed: false, interrupted: false, startedAt: minute(index), completeHistory: true }) }
}
function toolCall(index: number, output?: string): ToolCallItem {
  return { _tag: 'ToolCall', id: `turn/${index}/tool`, callId: `call-${index}`, name: 'read_file', input: { path: 'src/rows.ts' }, status: 'success', callSeen: true, at: minute(index), ...(output === undefined ? {} : { result: { content: output, isError: false, at: minute(index) } }) }
}
const historyTurn = (index: number, revise: { answer?: string; tool?: ToolCallItem } = {}) => turn(index, prompts[index % prompts.length], revise.answer ?? answers[index % answers.length], { tool: revise.tool ?? (index === 22 ? toolCall(22) : undefined) })
const initialTurns = [...Array.from({ length: 40 }, (_, index) => historyTurn(index)), turn(40, 'Ship the grouped rows.', undefined, { failed: true })]
const sync = { _tag: 'Live', since: now - 5000 } as const
const replacing = (next: TranscriptTurn) => (previous: readonly TranscriptTurn[]) => previous.map(entry => entry.id === next.id ? next : entry)

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
  const insertReply = React.useCallback(() => {
    const index = next.current++
    setTurns(previous => [...previous.slice(0, -1), turn(index, undefined, 'The visible rows are grouped and ready for review.  \nThe failed send keeps its original place.'), previous[previous.length - 1]!])
  }, [])
  const retry = React.useCallback(() => setRetries(count => count + 1), [])
  // Same length, different words: a length-only revision would miss it.
  const reviseAnswer = React.useCallback(() => setTurns(replacing(historyTurn(23, { answer: [...answers[23 % answers.length]!].reverse().join('') }))), [])
  const attachResult = React.useCallback(() => setTurns(replacing(historyTurn(22, { tool: toolCall(22, 'export const rows = groupVisible(filter(rows))') }))), [])
  return <main data-scheme={scheme} {...stylex.props(styles.root, ...baselineTheme, scheme === 'light' && lightTheme)}><EmbraceRuntimeProvider options={options}>
    <div {...stylex.props(styles.toolbar)}><Button onPress={republish} {...stylex.props(styles.button)}>Republish snapshot</Button><Button onPress={appendReply} {...stylex.props(styles.button)}>Append agent reply</Button><Button onPress={insertReply} {...stylex.props(styles.button)}>Insert reply above failed send</Button><Button onPress={reviseAnswer} {...stylex.props(styles.button)}>Revise answer</Button><Button onPress={attachResult} {...stylex.props(styles.button)}>Attach tool result</Button><span>Retries <output data-testid="retry-count">{retries}</output></span></div>
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
  await readerScrollsUp(viewport)
  await userEvent.click(canvas.getByRole('button', { name: 'Republish snapshot' }))
  await settleFrames()
  await expect(jump, 'same rows marked unread').not.toBeVisible()
  // Positive control: a new row still marks unread.
  await userEvent.click(canvas.getByRole('button', { name: 'Append agent reply' }))
  await waitFor(() => expect(jump).toBeVisible())
} }
export const SameRowsScrolledUpLight: Story = { ...SameRowsScrolledUp, args: { scheme: 'light' } }

async function readerScrollsUp(viewport: HTMLElement) {
  viewport.dispatchEvent(new WheelEvent('wheel', { deltaY: -viewport.scrollHeight }))
  viewport.scrollTop = 0
  await settleFrames()
}
const pointer = (target: Element, type: 'pointerdown' | 'pointerup', pointerId: number, pointerType: 'mouse' | 'touch') => target.dispatchEvent(new PointerEvent(type, { bubbles: true, cancelable: true, composed: true, pointerId, pointerType, isPrimary: pointerType === 'mouse', button: 0, buttons: type === 'pointerdown' ? 1 : 0, width: 1, height: 1, pressure: type === 'pointerdown' ? 0.5 : 0 }))

/** A press whose release the page never sees (the window blurs) does not keep the jump hidden. */
export const BlurEndsPress: Story = { play: async ({ canvasElement }) => {
  const { canvas, jump } = await ready(canvasElement)
  pointer(canvas.getByRole('button', { name: 'Retry' }), 'pointerdown', 1, 'mouse')
  window.dispatchEvent(new Event('blur'))
  canvas.getByRole('button', { name: 'Append agent reply' }).click()
  await waitFor(() => expect(jump, 'jump latched hidden after the window blurred').toBeVisible())
} }
export const BlurEndsPressLight: Story = { ...BlurEndsPress, args: { scheme: 'light' } }

/** With two pointers down, releasing one keeps the dock still; the reveal waits for the last release. */
export const SecondPointerHoldsDock: Story = { play: async ({ canvasElement }) => {
  const { viewport, canvas, jump } = await ready(canvasElement)
  const retry = canvas.getByRole('button', { name: 'Retry' })
  pointer(retry, 'pointerdown', 1, 'mouse')
  pointer(viewport, 'pointerdown', 2, 'touch')
  canvas.getByRole('button', { name: 'Append agent reply' }).click()
  await settleFrames()
  pointer(retry, 'pointerup', 1, 'mouse')
  await settleFrames()
  await expect(jump, 'jump revealed while a second pointer is down').not.toBeVisible()
  pointer(viewport, 'pointerup', 2, 'touch')
  await waitFor(() => expect(jump).toBeVisible())
} }
export const SecondPointerHoldsDockLight: Story = { ...SecondPointerHoldsDock, args: { scheme: 'light' } }

/** A same-length text revision is new content for a scrolled-up reader. */
export const SameLengthRevision: Story = { play: async ({ canvasElement }) => {
  const { viewport, canvas, jump } = await ready(canvasElement)
  await readerScrollsUp(viewport)
  await userEvent.click(canvas.getByRole('button', { name: 'Revise answer' }))
  await waitFor(() => expect(jump, 'same-length revision not marked unread').toBeVisible())
} }
export const SameLengthRevisionLight: Story = { ...SameLengthRevision, args: { scheme: 'light' } }

/** A result arriving for an existing tool call is new content for a scrolled-up reader. */
export const ToolResultArrives: Story = { play: async ({ canvasElement }) => {
  const { viewport, canvas, jump } = await ready(canvasElement)
  await readerScrollsUp(viewport)
  await userEvent.click(canvas.getByRole('button', { name: 'Attach tool result' }))
  await waitFor(() => expect(jump, 'tool result not marked unread').toBeVisible())
} }
export const ToolResultArrivesLight: Story = { ...ToolResultArrives, args: { scheme: 'light' } }

/** A reply inserted above the last failed send must not move Retry away from the pointer. */
export const InsertAbovePressedRow: Story = { play: async ({ canvasElement }) => {
  const { viewport, canvas, jump, retries } = await ready(canvasElement)
  const retry = canvas.getByRole('button', { name: 'Retry' })
  const row = retry.closest<HTMLElement>('[data-testid="user-message"]')!
  const top = row.getBoundingClientRect().top
  const height = viewport.scrollHeight
  await expect(viewport.scrollTop).toBeGreaterThan(viewport.clientHeight * 3)
  await pressAcross(retry, async () => {
    // Let pointerdown's history capture settle before inserting between that anchor and the pressed row.
    await settleFrames()
    canvas.getByRole('button', { name: 'Insert reply above failed send' }).click()
    await waitFor(() => expect(viewport.scrollHeight - height).toBeGreaterThan(80))
    await settleFrames()
    await expect(viewport.scrollHeight - height, 'reply should reproduce the roughly 107px insertion').toBeLessThan(140)
    await expect(Math.abs(row.getBoundingClientRect().top - top), 'pressed row moved under the pointer').toBeLessThanOrEqual(1)
    await expect(jump).not.toBeVisible()
  })
  await waitFor(() => expect(retries).toHaveTextContent('1'))
  await expect(Math.abs(row.getBoundingClientRect().top - top), 'pressed row snapped on release').toBeLessThanOrEqual(1)
  await waitFor(() => expect(jump).toBeVisible())
  await settleFrames()
  await expect(retries).toHaveTextContent('1')
  await expect(Math.abs(row.getBoundingClientRect().top - top), 'history anchor undid compensation after the dock settled').toBeLessThanOrEqual(1)
} }
export const InsertAbovePressedRowLight: Story = { ...InsertAbovePressedRow, args: { scheme: 'light' } }

/** Wheel scrolling during a press gives the reader ownership; compensation stays off. */
export const ScrollDuringPress: Story = { play: async ({ canvasElement }) => {
  const { viewport, canvas } = await ready(canvasElement)
  const retry = canvas.getByRole('button', { name: 'Retry' })
  pointer(retry, 'pointerdown', 1, 'mouse')
  viewport.dispatchEvent(new WheelEvent('wheel', { deltaY: -100 }))
  viewport.scrollTop -= 100
  await settleFrames()
  const row = retry.closest<HTMLElement>('[data-testid="user-message"]')!
  const top = row.getBoundingClientRect().top
  canvas.getByRole('button', { name: 'Insert reply above failed send' }).click()
  await settleFrames()
  await expect(row.getBoundingClientRect().top - top, 'press anchoring fought manual scrolling').toBeGreaterThan(80)
  pointer(viewport, 'pointerup', 1, 'mouse')
} }
export const ScrollDuringPressLight: Story = { ...ScrollDuringPress, args: { scheme: 'light' } }

type InsertSnapshot = 'retained' | 'fresh' | 'shifted'
const actionRows = Array.from({ length: 40 }, (_, index) => ({ id: `action-${index}`, version: `Row ${index}` }))
/** App-shaped clickable rows: keys are host ids, never array indexes or decoded object identities. */
function HostRowsPressStory({ scheme, snapshot, scrollOwner = 'lane' }: { scheme: Scheme; snapshot: InsertSnapshot; scrollOwner?: 'lane' | 'ancestor' | 'document' }) {
  const [rows, setRows] = React.useState(actionRows)
  const [activated, setActivated] = React.useState('')
  const insert = () => setRows(previous => {
    const current = snapshot === 'retained' ? previous : previous.map(row => ({ ...row }))
    // Stable host ids survive decoding a new snapshot and shifting the pressed row's index.
    const inserted = Array.from({ length: snapshot === 'shifted' ? 3 : 1 }, (_, index) => ({ id: `inserted-${index}`, version: `Inserted ${index}` }))
    const at = current.length - 1
    return [...current.slice(0, at), ...inserted, ...current.slice(at)]
  })
  const content = <EmbraceScrollViewport items={rows} data-testid="host-row-scroll" {...stylex.props(styles.hostViewport, scrollOwner !== 'lane' && styles.unboundedViewport)}>
    {rows.map((row, index) => <div key={row.id} data-item-id={row.id} data-row-index={index} {...stylex.props(styles.hostRow)}><Button onPress={() => setActivated(row.id)} {...stylex.props(styles.button)}>Activate {row.id}</Button></div>)}
  </EmbraceScrollViewport>
  return <main data-scroll-owner={scrollOwner} {...stylex.props(styles.root, scrollOwner === 'document' && styles.growingRoot, ...baselineTheme, scheme === 'light' && lightTheme)}>
    <div {...stylex.props(styles.toolbar)}><Button onPress={insert} {...stylex.props(styles.button)}>Insert host rows above</Button><output data-testid="activated-row">{activated}</output></div>
    {scrollOwner === 'ancestor' ? <div data-testid="scroll-owner" {...stylex.props(styles.hostViewport)}>{content}</div> : content}
  </main>
}
const hostInsertPlay: NonNullable<Story['play']> = async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  const viewport = canvas.getByTestId('host-row-scroll')
  const external = canvasElement.querySelector('[data-scroll-owner="lane"]') === null
  await document.fonts.ready
  await settleFrames()
  viewport.dispatchEvent(new WheelEvent('wheel', { deltaY: -viewport.scrollHeight }))
  const index = 39
  const id = `action-${index}`
  const action = canvas.getByRole('button', { name: `Activate ${id}`, exact: true })
  const row = action.closest<HTMLElement>('[data-item-id]')!
  // The failed outbox/action row is last; incoming server rows insert immediately above it.
  if (external) {
    action.scrollIntoView({ block: 'center' })
    await expect(viewport.scrollHeight - viewport.clientHeight, 'unbounded lane must have no scroll range').toBeLessThanOrEqual(1)
  } else viewport.scrollTop = viewport.scrollHeight
  await settleFrames()
  const before = row.getBoundingClientRect().top
  await expect(row).toHaveAttribute('data-row-index', String(index))
  const laneBottom = viewport.getBoundingClientRect().bottom
  const contentHeight = viewport.scrollHeight
  await pressAcross(action, async () => {
    await settleFrames()
    canvas.getByRole('button', { name: 'Insert host rows above' }).click()
    await waitFor(() => expect(canvas.getByRole('button', { name: 'Activate inserted-0' })).toBeInTheDocument())
    await settleFrames()
    await expect(canvas.getByRole('button', { name: `Activate ${id}`, exact: true }), 'host key must preserve the action DOM node').toBe(action)
    await expect(Number(row.dataset.rowIndex), 'same key must shift index').toBeGreaterThan(index)
    await expect(viewport.scrollHeight, 'incoming rows must grow the scrollable content').toBeGreaterThan(contentHeight)
    if (!external) await expect(Math.abs(viewport.getBoundingClientRect().bottom - laneBottom), 'bounded lane must remain the scroll owner during the press').toBeLessThanOrEqual(1)
    await expect(Math.abs(row.getBoundingClientRect().top - before), 'original action moved under the held pointer').toBeLessThanOrEqual(1)
  })
  await waitFor(() => expect(canvas.getByTestId('activated-row')).toHaveTextContent(id))
}
export const HostInsertRetainedRows: Story = { render: args => <HostRowsPressStory scheme={args.scheme} snapshot="retained" />, play: hostInsertPlay }
export const HostInsertFreshObjects: Story = { render: args => <HostRowsPressStory scheme={args.scheme} snapshot="fresh" />, play: hostInsertPlay }
export const HostInsertShiftedIndex: Story = { render: args => <HostRowsPressStory scheme={args.scheme} snapshot="shifted" />, play: hostInsertPlay }
export const HostInsertRetainedRowsLight: Story = { ...HostInsertRetainedRows, args: { scheme: 'light' } }
export const HostInsertFreshObjectsLight: Story = { ...HostInsertFreshObjects, args: { scheme: 'light' } }
export const HostInsertShiftedIndexLight: Story = { ...HostInsertShiftedIndex, args: { scheme: 'light' } }
const externalOwnerPlay: NonNullable<Story['play']> = async context => {
  const original = console.warn
  const warnings: string[] = []
  console.warn = (...args: unknown[]) => { warnings.push(String(args[0])); original(...args) }
  try {
    await hostInsertPlay(context)
    const action = within(context.canvasElement).getByRole('button', { name: 'Activate action-39', exact: true })
    pointer(action, 'pointerdown', 1, 'mouse')
    pointer(action, 'pointerup', 1, 'mouse')
    await expect(warnings.filter(text => text.startsWith('EmbraceScrollViewport:')), 'warn once per lane, not per press').toHaveLength(1)
  } finally { console.warn = original }
}
/** Defense in depth for a misconfigured host: the document, not the lane, scrolls. */
export const HostInsertDocumentScroll: Story = { render: args => <HostRowsPressStory scheme={args.scheme} snapshot="fresh" scrollOwner="document" />, play: externalOwnerPlay }
export const HostInsertDocumentScrollLight: Story = { ...HostInsertDocumentScroll, args: { scheme: 'light' } }
export const HostInsertAncestorScroll: Story = { render: args => <HostRowsPressStory scheme={args.scheme} snapshot="fresh" scrollOwner="ancestor" />, play: externalOwnerPlay }
export const HostInsertAncestorScrollLight: Story = { ...HostInsertAncestorScroll, args: { scheme: 'light' } }

const styles = stylex.create({
  root: { height: '100vh', width: '100%', boxSizing: 'border-box', display: 'flex', flexDirection: 'column', backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans },
  transcript: { flex: '1 1 0', minHeight: 0, display: 'flex', flexDirection: 'column' },
  toolbar: { display: 'flex', alignItems: 'center', gap: s.md, padding: s.md, flexShrink: 0, fontSize: t.metaSize },
  button: { minHeight: g.controlMd, paddingInline: s.md, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.sm, backgroundColor: surface.controlFill, color: ink.fg, fontFamily: t.fontSans, fontSize: t.metaSize, cursor: 'pointer', ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary } },
  hostViewport: { flex: '1 1 0', minHeight: 0, overflowY: 'auto' },
  hostRow: { minHeight: 72, display: 'flex', alignItems: 'center', paddingInline: s.md },
  growingRoot: { height: 'auto', display: 'block' },
  unboundedViewport: { flex: '0 0 auto', overflowY: 'visible' },
})
