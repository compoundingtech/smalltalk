import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { expect, fn, spyOn, userEvent, waitFor, within } from 'storybook/test'
import { Transcript, type TranscriptAvailability, type TranscriptHistory, type TranscriptTurn } from './assistant-ui/composition/Transcript'
import type { TranscriptEmptyState } from './assistant-ui/composition/TranscriptFeedback'
import { EmbraceRuntimeProvider } from './assistant-ui/EmbraceRuntime'
import { EmbraceComposer } from './assistant-ui/EmbraceComposer'
import type { ConversationItem, SendState, TextItem } from './assistant-ui/embrace-data/model'
import { workLogTurnFromItems, type WorkLogCall } from './assistant-ui/taste/work-log'
import { WorkLogV1 } from './assistant-ui/taste/WorkLogV1'
import type { SyncStatus } from './assistant-ui/st3-views/sync-status'
import { baselineTheme } from './assistant-ui/neutral-theme'
import { lightTheme, type Scheme } from './assistant-ui/composition-theme'
import { surfaceVars as surface, textVars as ink, spaceVars as s, typeVars as t, geometryVars as g } from './assistant-ui/composition-tokens.stylex'

const now = Date.parse('2026-01-15T12:01:00Z')
const at = '2026-01-15T12:00:00Z'
const source = 'export const visibleRows = rows.filter(row => row.visible)\nexport const count = visibleRows.length\nexport const labels = visibleRows.map(row => row.label)\nexport const selected = visibleRows.find(row => row.selected)\nexport const first = visibleRows[0]\nexport const last = visibleRows.at(-1)\n'
const answer = 'The row projection now keeps **visible rows** together and preserves selection.\n\n```ts\nconst count = visibleRows.length\n```\n\nThe focused checks passed.'
type State = 'settled' | 'expanded' | 'streaming' | 'failed' | 'interrupted' | 'unknown' | 'loading' | 'catching-up' | 'reconnecting' | 'sync-failed' | 'pending-send' | 'failed-send' | 'empty'
interface TranscriptStoryData { readonly turns: readonly TranscriptTurn[]; readonly sync: SyncStatus; readonly availability?: TranscriptAvailability; readonly history?: TranscriptHistory; readonly emptyState?: React.ReactNode | TranscriptEmptyState }
function fixture(state: State, prefix: string): TranscriptStoryData {
  const running = state === 'streaming', failed = state === 'failed', interrupted = state === 'interrupted'
  const prompt: TextItem & { role: 'user' } = { _tag: 'Text', id: `${prefix}/prompt`, role: 'user', text: 'Keep the row projection readable and verify that selection survives the change.', attachments: [], streaming: false, at, sender: { kind: 'human', label: 'Operator' }, sendState: state === 'pending-send' ? { _tag: 'Pending' } : state === 'failed-send' ? { _tag: 'Failed', reason: { _tag: 'Failed' }, detail: 'The connection dropped while sending. Your draft is still saved; try again in a moment.' } : undefined }
  const calls: ConversationItem[] = [
    { _tag: 'ToolCall', id: `${prefix}/read`, callId: `${prefix}/read`, name: 'read', input: { path: 'src/rows.ts' }, status: 'success', result: { content: source, mediaType: 'text/plain', isError: false, at: '2026-01-15T12:00:03Z' }, callSeen: true, at: '2026-01-15T12:00:01Z' },
    { _tag: 'ToolCall', id: `${prefix}/run`, callId: `${prefix}/run`, name: 'run', input: { command: 'pnpm test rows' }, status: failed ? 'error' : interrupted ? 'interrupted' : running ? 'running' : 'success', result: running ? undefined : { content: failed ? 'The row assertion did not match the observed selection.' : interrupted ? 'Stopped before the checks completed.' : '✓ selection retained\n✓ visible count matches\n✓ empty rows handled', mediaType: 'text/x-shellscript', isError: failed, at: '2026-01-15T12:00:18Z' }, callSeen: true, at: '2026-01-15T12:00:05Z' },
    { _tag: 'ToolCall', id: `${prefix}/empty`, callId: `${prefix}/empty`, name: 'read', input: { path: 'src/empty.ts' }, status: 'success', result: { content: '', isError: false, at: '2026-01-15T12:00:19Z' }, callSeen: true, at: '2026-01-15T12:00:19Z' },
  ]
  const reasoning: ConversationItem = { _tag: 'Reasoning', id: `${prefix}/thinking`, text: 'Compare the **observed selection** with the projected rows before changing the result.', streaming: running, at }
  const response: TextItem = { _tag: 'Text', id: `${prefix}/answer`, role: 'assistant', text: running ? 'The projection keeps **visible rows** together. Checking the final `selection`' : answer, attachments: [], streaming: running, at: '2026-01-15T12:00:24Z', sender: { kind: 'agent', label: 'Assistant' } }
  const items = [...calls, reasoning, ...(failed || interrupted ? [] : [response])]
  const work = workLogTurnFromItems([prompt, ...items], { kindFor: name => name === 'run' ? 'run' : 'read', running, failed, interrupted, durationMs: running || failed || interrupted ? undefined : 24000, startedAt: at, completeHistory: state !== 'unknown', failureNote: failed ? 'The run stopped; your conversation and changes are retained.' : undefined })
  const sync: SyncStatus = state === 'loading' ? { _tag: 'Connecting', attempt: 1, since: now - 8000 }
    : state === 'catching-up' ? { _tag: 'Progress', stage: 'reading', elapsedMs: 1500, stageSince: now - 1500, reportedAt: now, done: 32, total: 64 }
    : state === 'reconnecting' ? { _tag: 'Stale', reason: { _tag: 'Reconnecting', attempt: 2, nextAt: now + 3000, issue: 'Connection interrupted' }, lastLiveAt: now - 10000 }
    : state === 'sync-failed' ? { _tag: 'Failed', cause: { _tag: 'Server', code: 'subscription-limit', message: 'Subscription unavailable' } }
    : { _tag: 'Live', since: now - 5000 }
  return { turns: state === 'loading' || state === 'empty' ? [] : [{ id: prefix, prompt, items, work, senderCaptions: { [response.id]: 'Assistant' } }], sync }
}
const cases: Readonly<Record<State, TranscriptStoryData>> = {
  settled: fixture('settled', 'settled'), expanded: fixture('expanded', 'expanded'), streaming: fixture('streaming', 'streaming'), failed: fixture('failed', 'failed'), interrupted: fixture('interrupted', 'interrupted'), unknown: fixture('unknown', 'unknown'), loading: fixture('loading', 'loading'), 'catching-up': fixture('catching-up', 'catching-up'), reconnecting: fixture('reconnecting', 'reconnecting'), 'sync-failed': fixture('sync-failed', 'sync-failed'), 'pending-send': fixture('pending-send', 'pending-send'), 'failed-send': fixture('failed-send', 'failed-send'), empty: fixture('empty', 'empty'),
}
/** `drop` withholds ids from the runtime adapter, standing in for an adapter that never adopts them. */
function RuntimeTranscript({ data, drop, onOpenTool, onRetry, onRetrySend, availability, history, emptyState }: { data: TranscriptStoryData; drop?: readonly string[]; onOpenTool?: (call: WorkLogCall) => void; onRetry: () => void; onRetrySend?: (itemId: string) => void; availability?: TranscriptAvailability; history?: TranscriptHistory; emptyState?: React.ReactNode | TranscriptEmptyState }) {
  const messages = React.useMemo(() => data.turns.flatMap(turn => turn.prompt === undefined ? turn.items : [turn.prompt, ...turn.items]).filter(item => !drop?.includes(item.id)), [data, drop])
  const options = React.useMemo(() => ({ messages, isRunning: data.turns.some(turn => turn.work.running), onNew: async () => {} }), [messages, data])
  return <EmbraceRuntimeProvider options={options}><Transcript title="Row projection" turns={data.turns} sync={data.sync} now={now} observedAt={now - 8000} onOpenTool={onOpenTool} onRetryRun={onRetry} onRetrySync={onRetry} onRetrySend={onRetrySend} availability={availability} history={history} emptyState={emptyState} /></EmbraceRuntimeProvider>
}
function TranscriptStory({ scheme = 'dark', state = 'settled', availability, history, emptyState }: { scheme?: Scheme; state?: State; availability?: TranscriptAvailability; history?: TranscriptHistory; emptyState?: React.ReactNode | TranscriptEmptyState }) {
  const [opened, setOpened] = React.useState<WorkLogCall | undefined>(undefined)
  const [retried, setRetried] = React.useState(false)
  const retry = React.useCallback(() => setRetried(true), [])
  return <section aria-label={`${state} conversation example`} data-scheme={scheme} {...stylex.props(styles.root, ...baselineTheme, scheme === 'light' && lightTheme)}><div {...stylex.props(styles.frame)}><RuntimeTranscript data={cases[state]} onOpenTool={setOpened} onRetry={retry} availability={availability} history={history} emptyState={emptyState} /></div>{opened !== undefined && <section aria-label="Opened tool detail" {...stylex.props(styles.detail)}><strong>{opened.title} {opened.argsSummary}</strong><pre>{opened.detail}</pre></section>}{retried && <p role="status">Retry requested by the host.</p>}</section>
}
const meta = { title: 'Fractal UI/Transcript', component: TranscriptStory, parameters: { layout: 'fullscreen' }, args: { scheme: 'dark', state: 'settled' }, argTypes: { scheme: { options: ['dark', 'light'], control: 'radio' }, state: { options: Object.keys(cases), control: 'select' } } } satisfies Meta<typeof TranscriptStory>
export default meta
type Story = StoryObj<typeof meta>
const adjacentReasoning = Array.from({ length: 11 }, (_, index): ConversationItem => ({
  _tag: 'Reasoning', id: `reasoning-run/${index}`, text: `Check ${index + 1}: keep the observed reasoning in order.`, streaming: false, at,
}))
const adjacentReasoningData: TranscriptStoryData = {
  sync: { _tag: 'Live', since: now },
  turns: [{ id: 'reasoning-run', items: adjacentReasoning, work: workLogTurnFromItems(adjacentReasoning, { kindFor: () => 'read', running: false, failed: false, interrupted: false, completeHistory: true }) }],
}
export const AdjacentReasoning: Story = {
  render: args => <section {...stylex.props(styles.root, ...baselineTheme, args.scheme === 'light' && lightTheme)}><div {...stylex.props(styles.frame)}><RuntimeTranscript data={adjacentReasoningData} onRetry={() => {}} /></div></section>,
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement)
    await userEvent.click(await canvas.findByRole('button', { name: 'Worked' }))
    await expect(canvas.getAllByRole('button', { name: 'Thinking' })).toHaveLength(1)
  },
}
export const Settled: Story = { play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  const fold = await canvas.findByRole('button', { name: /Worked for 24s/ })
  await expect(fold).toHaveAttribute('aria-expanded', 'false')
  await expect(canvasElement.querySelector('[data-testid="tool-detail-preview"]')).toBeNull()
  await expect(canvasElement.querySelector('[data-testid="answer-meta"]')).not.toBeNull()
  await expect(canvasElement.querySelector('[data-testid="live-work"]')).toBeNull()
} }
export const SettledLight: Story = { ...Settled, args: { scheme: 'light' } }
export const Expanded: Story = { args: { state: 'expanded' }, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  const fold = await canvas.findByRole('button', { name: /Worked for 24s/ })
  await userEvent.click(fold)
  await expect(fold).toHaveAttribute('aria-expanded', 'true')
  await expect(canvas.getByTestId('tool-preview-remaining')).toHaveTextContent('+2 lines')
  const previews = canvas.getAllByTestId('tool-detail-preview')
  await expect(previews).toHaveLength(2)
  await waitFor(() => expect(previews[0]!.querySelector('[data-syntax-token~="keyword"]')).not.toBeNull())
  await expect(previews[0]).toHaveTextContent('export const visibleRows')
  await expect(previews[1]).toHaveTextContent('selection retained')
  await expect(canvas.getByTestId('work-log-divider')).toBeInTheDocument()
  await expect(canvas.getByRole('button', { name: 'Thinking' })).toHaveAttribute('aria-expanded', 'false')
  const empty = canvasElement.querySelector('[data-tool-status="success"]:last-child')!
  await expect(empty.querySelector('button')).toBeNull()
  await expect(empty).toHaveTextContent('No output')
  await userEvent.click(canvas.getByRole('button', { name: 'Open read tool detail' }))
  await expect(canvas.getByRole('region', { name: 'Opened tool detail' }).querySelector('pre')?.textContent).toBe(source)
} }
export const ExpandedLight: Story = { ...Expanded, args: { state: 'expanded', scheme: 'light' } }
const nativeOutputItems: readonly ConversationItem[] = [
  { name: 'string output', content: 'String result retained.' },
  { name: 'native array output', content: [{ type: 'text', text: 'First native line.' }, { type: 'image', data: 'synthetic-bytes' }, { type: 'text', text: 'Second native line.' }] },
  { name: 'empty output', content: [] },
  { name: 'non-text output', content: [{ type: 'image', data: 'synthetic-bytes' }] },
].map(({ name, content }, index) => ({ _tag: 'ToolCall', id: `native-output/${index}`, callId: `native-call/${index}`, name, input: {}, status: 'success', callSeen: true, at, result: { content, isError: false, at } }))
const nativeOutputTurn = workLogTurnFromItems(nativeOutputItems, { kindFor: () => 'read', running: false, failed: false, interrupted: false, completeHistory: true })
export const NativeToolOutput: Story = { render: args => <main {...stylex.props(styles.root, ...baselineTheme, args.scheme === 'light' && lightTheme)}><WorkLogV1 turn={nativeOutputTurn} /></main>, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  await userEvent.click(canvas.getByRole('button', { name: 'Worked' }))
  const native = canvas.getByRole('button', { name: /^native array output/ })
  await userEvent.click(native)
  await expect(native).toHaveAttribute('aria-expanded', 'true')
  await expect(canvas.getByTestId('work-call-output')).toHaveTextContent('First native line.')
  await expect(canvas.getByTestId('work-call-output')).toHaveTextContent('Second native line.')
  await expect(native).not.toHaveTextContent('No output')
  await userEvent.click(canvas.getByRole('button', { name: /^string output/ }))
  await expect(canvas.getAllByTestId('work-call-output')).toHaveLength(2)
  await expect(canvas.getAllByTestId('work-call-output')[0]).toHaveTextContent('String result retained.')
  await expect(canvas.queryByRole('button', { name: /empty output|non-text output/ })).toBeNull()
  await expect(canvas.getAllByText(/No output/)).toHaveLength(2)
} }
export const NativeToolOutputLight: Story = { ...NativeToolOutput, args: { scheme: 'light' } }
export const Streaming: Story = { args: { state: 'streaming' }, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  await canvas.findByTestId('live-work')
  await expect(canvasElement.querySelectorAll('[data-testid="live-work"]')).toHaveLength(1)
  await expect(canvas.getByRole('status', { name: 'Response in progress' })).toBeInTheDocument()
  await expect(canvas.getByRole('progressbar', { name: 'Run in progress' })).toHaveAttribute('aria-valuetext', 'Running')
  await expect(canvasElement.querySelectorAll('[data-streaming-tail="true"]')).toHaveLength(1)
  await expect(canvasElement.querySelector('[data-testid="answer-meta"]')).toBeNull()
  await expect(canvas.getByRole('button', { name: /Working/ })).toBeDisabled()
} }
export const StreamingLight: Story = { ...Streaming, args: { state: 'streaming', scheme: 'light' } }
export const Failed: Story = { args: { state: 'failed' }, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  const notice = await canvas.findByRole('alert')
  await expect(notice).toHaveTextContent('Command did not complete')
  await expect(canvas.getByTestId('transcript-turn')).toContainElement(notice)
  await expect(canvas.getByTestId('transcript-scroll')).toContainElement(notice)
  await expect(notice.closest('[data-error-overlay-layer]')).toBeNull()
  await userEvent.click(canvas.getByRole('button', { name: 'Open output' }))
  await expect(canvas.getByRole('region', { name: 'Opened tool detail' })).toHaveTextContent('assertion did not match')
  await userEvent.click(canvas.getByRole('button', { name: 'Dismiss: Command did not complete' }))
  await expect(canvas.queryByRole('alert')).toBeNull()
  await expect(canvas.getByTestId('transcript-turn')).toBeInTheDocument()
} }
export const FailedLight: Story = { ...Failed, args: { state: 'failed', scheme: 'light' } }
export const Interrupted: Story = { args: { state: 'interrupted' } }
export const InterruptedLight: Story = { args: { state: 'interrupted', scheme: 'light' } }
export const CompletionUnknown: Story = { args: { state: 'unknown' } }
export const Loading: Story = { args: { state: 'loading' }, play: async ({ canvasElement }) => {
  await expect(await within(canvasElement).findByText('Loading conversation…')).toBeVisible()
} }
export const LoadingLight: Story = { ...Loading, args: { state: 'loading', scheme: 'light' } }
export const CatchingUp: Story = { args: { state: 'catching-up' }, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  await canvas.findByTestId('transcript-turn')
  await expect(canvas.getByRole('progressbar', { name: 'Thread synchronization' })).toHaveAttribute('aria-valuenow', '32')
  await expect(canvas.getByRole('progressbar', { name: 'Thread synchronization' })).toHaveAttribute('aria-valuemax', '64')
  await expect(canvas.getByTestId('answer-meta')).toBeInTheDocument()
} }
export const CatchingUpLight: Story = { ...CatchingUp, args: { state: 'catching-up', scheme: 'light' } }
export const Reconnecting: Story = { args: { state: 'reconnecting' } }
export const SyncFailed: Story = { args: { state: 'sync-failed' } }
export const SyncFailedLight: Story = { args: { state: 'sync-failed', scheme: 'light' } }
export const AllStates: Story = { render: args => <main {...stylex.props(styles.all, ...baselineTheme, args.scheme === 'light' && lightTheme)}>{(['settled', 'streaming', 'failed', 'interrupted', 'unknown', 'loading', 'catching-up', 'reconnecting', 'sync-failed'] as const).map(state => <section key={state}><h2>{state}</h2><TranscriptStory scheme={args.scheme} state={state} /></section>)}</main>, play: async ({ canvasElement }) => {
  await expect(within(canvasElement).getAllByRole('main')).toHaveLength(1)
  await expect(canvasElement.querySelector('main main')).toBeNull()
  const examples = within(canvasElement).getAllByRole('region', { name: / conversation example$/ })
  await expect(examples).toHaveLength(9)
  await expect(new Set(examples.map(example => example.getAttribute('aria-label'))).size).toBe(9)
} }
export const AllStatesLight: Story = { ...AllStates, args: { scheme: 'light' } }
const metadataCases = ['known', 'unknown', 'streaming'] as const
function AnswerMetadataStory({ scheme = 'dark' }: { scheme?: Scheme }) {
  const data = React.useMemo(() => ({ sync: { _tag: 'Live', since: now } as const, turns: metadataCases.map(kind => {
    const base = fixture('settled', `metadata-${kind}`).turns[0]!
    const response = base.items.find((item): item is TextItem => item._tag === 'Text' && item.role === 'assistant')!
    const updated = { ...response, at: kind === 'unknown' ? '' : response.at, streaming: kind === 'streaming' }
    return { ...base, items: [updated], work: { ...base.work, calls: [], running: kind === 'streaming' } }
  }) }), [])
  const open = React.useCallback((_call: WorkLogCall) => {}, [])
  const retry = React.useCallback(() => {}, [])
  return <main {...stylex.props(styles.root, ...baselineTheme, scheme === 'light' && lightTheme)}><RuntimeTranscript data={data} onOpenTool={open} onRetry={retry} /></main>
}
export const SettledAnswerMeta: Story = { render: args => <AnswerMetadataStory scheme={args.scheme} />, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  await canvas.findAllByRole('button', { name: 'Copy answer' })
  const answers = canvas.getAllByTestId('agent-message')
  await expect(answers).toHaveLength(3)
  await expect(answers[0]!.querySelector('time')?.getAttribute('datetime')).toBe('2026-01-15T12:00:24.000Z')
  await expect(answers[1]!.querySelector('time')).toBeNull()
  await expect(answers[2]!.querySelector('[data-testid="answer-meta"]')).toBeNull()
  const footer = answers[0]!.querySelector('[data-testid="answer-meta"]')!
  const prose = answers[0]!.querySelector('[data-testid="markdown"]')!
  await expect(footer.getBoundingClientRect().top).toBeGreaterThanOrEqual(prose.getBoundingClientRect().bottom)
  await expect(getComputedStyle(footer).opacity).toBe('1')
  const clipboard = navigator.clipboard
  const original = Object.getOwnPropertyDescriptor(clipboard, 'writeText')
  let copied = ''
  Object.defineProperty(clipboard, 'writeText', { configurable: true, value: async (text: string) => { copied = text } })
  try { await userEvent.click(within(answers[0]!).getByRole('button', { name: 'Copy answer' })); await expect(copied).toBe(answer) }
  finally { if (original === undefined) delete (clipboard as unknown as Record<string, unknown>)['writeText']; else Object.defineProperty(clipboard, 'writeText', original) }
} }
const unavailableReason = 'This conversation is not available right now.'
// Raw host diagnostics: never shown in the state itself, only behind "Show details".
const unavailableDetail = 'subscription-limit: Subscription unavailable'
const unavailable = (action?: { readonly label: string; readonly onPress: () => void }): TranscriptAvailability => ({ _tag: 'Unavailable', reason: unavailableReason, detail: unavailableDetail, ...(action === undefined ? {} : { action }) })
/** One reason line, no raw error text, no sync line beside the state and exactly the host's recovery action. */
async function expectUnavailableState(canvasElement: HTMLElement, action?: string) {
  const state = await within(canvasElement).findByTestId('transcript-unavailable')
  await expect(canvasElement.querySelector('[data-testid="sync-line"]')).toBeNull()
  await expect(canvasElement.querySelector('[data-testid="transcript-turn"]')).toBeNull()
  await expect([...state.querySelectorAll('p')].filter(line => line.checkVisibility()).map(line => line.textContent)).toEqual([unavailableReason])
  await expect(within(state).queryByText(unavailableDetail)?.checkVisibility() ?? false).toBe(false)
  await expect(within(state).queryAllByRole('button').filter(button => button.textContent !== 'Show details').map(button => button.textContent)).toEqual(action === undefined ? [] : [action])
  return state
}
async function tabTo(target: HTMLElement) {
  for (let step = 0; step < 12 && document.activeElement !== target; step++) await userEvent.tab()
  await expect(target).toHaveFocus()
}
export const Unavailable: Story = { args: { state: 'sync-failed', availability: unavailable() }, play: async ({ canvasElement }) => {
  const state = await expectUnavailableState(canvasElement)
  await userEvent.click(within(state).getByRole('button', { name: 'Show details' }))
  await waitFor(() => expect(within(state).getByText(unavailableDetail).checkVisibility()).toBe(true))
} }
export const UnavailableLight: Story = { ...Unavailable, args: { ...Unavailable.args, scheme: 'light' } }
const tryAgain = fn()
export const UnavailableWithAction: Story = { args: { state: 'sync-failed', availability: unavailable({ label: 'Try again', onPress: tryAgain }) }, play: async ({ canvasElement }) => {
  tryAgain.mockClear()
  const state = await expectUnavailableState(canvasElement, 'Try again')
  const action = within(state).getByRole('button', { name: 'Try again' })
  await tabTo(action)
  await userEvent.keyboard('{Enter}')
  await expect(tryAgain).toHaveBeenCalledTimes(1)
  await userEvent.click(action)
  await expect(tryAgain).toHaveBeenCalledTimes(2)
} }
export const UnavailableWithActionLight: Story = { ...UnavailableWithAction, args: { ...UnavailableWithAction.args, scheme: 'light' } }
const noMessages = { messages: [], isRunning: false, onNew: async () => {} }
/** The host's composer stays a sibling of the unavailable Transcript in the thread column. */
function UnavailableComposerStory({ scheme = 'dark' }: { scheme?: Scheme }) {
  return <main data-scheme={scheme} {...stylex.props(styles.root, ...baselineTheme, scheme === 'light' && lightTheme)}><EmbraceRuntimeProvider options={noMessages}><section aria-label="Conversation" {...stylex.props(styles.thread)}>
    <Transcript title="Row projection" turns={[]} sync={cases['sync-failed'].sync} now={now} observedAt={now - 8000} availability={unavailable({ label: 'Try again', onPress: tryAgain })} />
    <div data-testid="host-composer" {...stylex.props(styles.composerDock)}><EmbraceComposer variant="C1" /></div>
  </section></EmbraceRuntimeProvider></main>
}
export const UnavailableWithComposer: Story = { render: args => <UnavailableComposerStory scheme={args.scheme} />, play: async ({ canvasElement }) => {
  tryAgain.mockClear()
  const state = await expectUnavailableState(canvasElement, 'Try again')
  const dock = within(canvasElement).getByTestId('host-composer')
  const input = within(dock).getByRole('textbox', { name: 'Message' })
  await expect(input).toBeVisible()
  await expect(input).toBeEnabled()
  await expect(state.getBoundingClientRect().bottom).toBeLessThanOrEqual(dock.getBoundingClientRect().top + 0.5)
  await expect(dock.getBoundingClientRect().bottom).toBeLessThanOrEqual(window.innerHeight + 0.5)
  await expect(dock.getBoundingClientRect().height).toBeGreaterThan(0)
  await tabTo(input)
  await userEvent.keyboard('Still here')
  await expect(input).toHaveValue('Still here')
} }
export const UnavailableWithComposerLight: Story = { ...UnavailableWithComposer, args: { scheme: 'light' } }
const loadEarlier = fn()
export const HasOlderWithLoad: Story = { args: { history: { _tag: 'HasOlder', onLoadEarlier: loadEarlier } }, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  loadEarlier.mockClear()
  await canvas.findByRole('button', { name: /Worked for 24s/ })
  await expect(canvas.getByTestId('history-boundary')).toHaveTextContent('Earlier messages not loaded')
  await userEvent.click(canvas.getByRole('button', { name: 'Load earlier messages' }))
  await expect(loadEarlier).toHaveBeenCalledTimes(1)
} }
export const HasOlderWithLoadLight: Story = { ...HasOlderWithLoad, args: { ...HasOlderWithLoad.args, scheme: 'light' } }
export const HasOlderWithoutLoad: Story = { args: { history: { _tag: 'HasOlder' } }, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  await canvas.findByRole('button', { name: /Worked for 24s/ })
  await expect(canvas.getByTestId('history-boundary')).toHaveTextContent('Earlier messages not loaded')
  await expect(canvas.queryByRole('button', { name: 'Load earlier messages' })).toBeNull()
} }
export const HasOlderLight: Story = { ...HasOlderWithoutLoad, args: { ...HasOlderWithoutLoad.args, scheme: 'light' } }
export const PendingSend: Story = { args: { state: 'pending-send' }, play: async ({ canvasElement }) => {
  const prompt = await within(canvasElement).findByTestId('user-message')
  await expect(prompt).toHaveAttribute('data-send-state', 'pending')
  await expect(prompt.querySelector('time')).toBeNull()
} }
export const PendingSendLight: Story = { ...PendingSend, args: { state: 'pending-send', scheme: 'light' } }
export const FailedSend: Story = { args: { state: 'failed-send' }, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  const failure = await canvas.findByTestId('send-failure')
  await expect(failure).toHaveTextContent("Couldn't send")
  await userEvent.click(canvas.getByRole('button', { name: "Couldn't send" }))
  await expect(failure).toHaveTextContent('The connection dropped while sending')
} }
export const FailedSendLight: Story = { ...FailedSend, args: { state: 'failed-send', scheme: 'light' } }
/** The server echo replaces a pending prompt in place: same item id, one row, no duplicate. */
function SendIdentityStory({ scheme = 'dark' }: { scheme?: Scheme }) {
  const [delivered, setDelivered] = React.useState(false)
  const data = React.useMemo(() => fixture(delivered ? 'settled' : 'pending-send', 'identity'), [delivered])
  return <main data-scheme={scheme} {...stylex.props(styles.root, ...baselineTheme, scheme === 'light' && lightTheme)}><button type="button" onClick={() => setDelivered(true)}>Deliver echo</button><div {...stylex.props(styles.frame)}><RuntimeTranscript data={data} onOpenTool={() => {}} onRetry={() => {}} /></div></main>
}
export const SendIdentity: Story = { render: args => <SendIdentityStory scheme={args.scheme} />, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  const selector = '[data-testid="user-message"][data-item-id="identity/prompt"]'
  await expect(await canvas.findByTestId('user-message')).toHaveAttribute('data-send-state', 'pending')
  const pending = canvasElement.querySelector(selector)
  const fold = await canvas.findByRole('button', { name: /Worked for 24s/ })
  await userEvent.click(fold)
  await expect(fold).toHaveAttribute('aria-expanded', 'true')
  const removed: Node[] = []
  const recordRemovals = (records: MutationRecord[]) => {
    for (const record of records) for (const node of record.removedNodes) {
      if (node instanceof Element && (node.matches('[data-testid="user-message"], [data-testid="transcript-turn"]') || node.querySelector('[data-testid="user-message"], [data-testid="transcript-turn"]') !== null)) removed.push(node)
    }
  }
  const observer = new MutationObserver(recordRemovals)
  observer.observe(canvasElement, { subtree: true, childList: true })
  try {
    await userEvent.click(canvas.getByRole('button', { name: 'Deliver echo' }))
    await waitFor(() => expect(canvasElement.querySelector(selector)).toHaveAttribute('data-send-state', 'sent'))
    recordRemovals(observer.takeRecords())
    await expect(removed).toHaveLength(0)
    await expect(canvasElement.querySelector(selector)).toBe(pending)
    await expect(canvas.getByRole('button', { name: /Worked for 24s/ })).toHaveAttribute('aria-expanded', 'true')
    await expect(canvasElement.querySelectorAll(selector)).toHaveLength(1)
    await expect(canvasElement.querySelectorAll('[data-testid="user-message"]')).toHaveLength(1)
  } finally { observer.disconnect() }
} }
export const SendIdentityLight: Story = { ...SendIdentity, args: { scheme: 'light' } }

const appendBase = fixture('streaming', 'append')
const appendTurn = appendBase.turns[0]!
const appendInitialItems: readonly ConversationItem[] = [
  appendTurn.items[0]!, appendTurn.items[3]!,
  { ...(appendTurn.items[4] as TextItem), text: Array.from({ length: 24 }, (_, index) => `Observed row ${index + 1} stays in the retained transcript.`).join('\n\n') },
]
const appendItems: readonly ConversationItem[] = [
  { _tag: 'ToolCall', id: 'append/new-tool', callId: 'append/new-tool', name: 'run', input: { command: "printf '%s\\n' 'retained rows'" }, status: 'running', callSeen: true, at },
  { _tag: 'Reasoning', id: 'append/new-thinking', text: 'New reasoning follows the observed tool call.', streaming: true, at },
  { _tag: 'Text', id: 'append/new-answer', role: 'assistant', text: 'New answer content follows the retained rows.', attachments: [], streaming: true, at },
]
function AppendedItemsStory({ scheme = 'dark' }: { scheme?: Scheme }) {
  const [count, setCount] = React.useState(0)
  const data = React.useMemo<TranscriptStoryData>(() => {
    const items = [...appendInitialItems, ...appendItems.slice(0, count)]
    return { sync: appendBase.sync, turns: [{ ...appendTurn, items, work: workLogTurnFromItems(items, { kindFor: name => name === 'run' ? 'run' : 'read', running: true, failed: false, interrupted: false, startedAt: at, completeHistory: true }) }] }
  }, [count])
  return <main data-scheme={scheme} {...stylex.props(styles.root, ...baselineTheme, scheme === 'light' && lightTheme)}><button type="button" disabled={count === appendItems.length} onClick={() => setCount(value => value + 1)}>Append {count === 0 ? 'tool' : count === 1 ? 'reasoning' : 'answer'}</button><div {...stylex.props(styles.frame)}><RuntimeTranscript data={data} onOpenTool={() => {}} onRetry={() => {}} /></div></main>
}
export const AppendedItems: Story = { render: args => <AppendedItemsStory scheme={args.scheme} />, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  const turn = await canvas.findByTestId('transcript-turn')
  const prompt = await canvas.findByTestId('user-message')
  const answerRow = await canvas.findByTestId('agent-message')
  const work = within(turn).getByTestId('work-log')
  const tool = within(work).getByRole('button', { name: /^read src\/rows\.ts\b/ })
  await userEvent.click(tool)
  await expect(tool).toHaveAttribute('aria-expanded', 'true')
  const thinking = within(work).getByTestId('thinking-entry')
  const thinkingControl = within(thinking).getByRole('button', { name: 'Thinking' })
  await userEvent.click(thinkingControl)
  await expect(thinkingControl).toHaveAttribute('aria-expanded', 'true')
  const lane = canvas.getByTestId('transcript-scroll')
  lane.dispatchEvent(new WheelEvent('wheel', { deltaY: -400 }))
  lane.scrollTop = 48
  lane.dispatchEvent(new Event('scroll'))
  const scrollTop = lane.scrollTop
  await expect(scrollTop).toBeGreaterThan(0)
  const retained = [turn, prompt, answerRow, work, tool, thinking]
  const removed: Node[] = []
  // A transient append must stay pending; a fallback row appearing even briefly means it was treated as stranded.
  const fallbacks: Node[] = []
  const recordRemovals = (records: MutationRecord[]) => {
    for (const record of records) {
      for (const node of record.removedNodes) if (retained.some(element => node === element || node.contains(element))) removed.push(node)
      for (const node of record.addedNodes) if (node instanceof Element && (node.matches('[data-testid="transcript-stranded"]') || node.querySelector('[data-testid="transcript-stranded"]') !== null)) fallbacks.push(node)
    }
  }
  const observer = new MutationObserver(recordRemovals)
  observer.observe(canvasElement, { subtree: true, childList: true })
  try {
    for (const [index, label] of ['tool', 'reasoning', 'answer'].entries()) {
      await userEvent.click(canvas.getByRole('button', { name: `Append ${label}` }))
      await waitFor(() => expect(index === 0 ? work.querySelector('[data-tool-status="running"]') : index === 1 ? within(work).queryAllByTestId('thinking-entry').length === 2 : within(turn).queryByText('New answer content follows the retained rows.')).toBeTruthy())
      recordRemovals(observer.takeRecords())
      await expect(removed).toHaveLength(0)
      await expect(fallbacks).toHaveLength(0)
      await expect(canvas.getByTestId('transcript-turn')).toBe(turn)
      await expect(canvas.getByTestId('user-message')).toBe(prompt)
      await expect(canvasElement.querySelector('[data-item-id="append/answer"]')).toBe(answerRow)
      await expect(within(turn).getByTestId('work-log')).toBe(work)
      await expect(within(work).getByRole('button', { name: /^read src\/rows\.ts\b/ })).toBe(tool)
      await expect(within(work).getAllByTestId('thinking-entry')[0]).toBe(thinking)
      await expect(tool).toHaveAttribute('aria-expanded', 'true')
      await expect(thinkingControl).toHaveAttribute('aria-expanded', 'true')
      await expect(lane.scrollTop).toBe(scrollTop)
    }
  } finally { observer.disconnect() }
} }
export const AppendedItemsLight: Story = { ...AppendedItems, args: { scheme: 'light' } }

const strandedPrompt: TextItem = { _tag: 'Text', id: 'stranded/prompt', role: 'user', text: 'Summarize the retained rows.', attachments: [], streaming: false, at }
const strandedItems: readonly ConversationItem[] = [
  { _tag: 'Text', id: 'stranded/lost-answer', role: 'assistant', text: 'This answer never reached the runtime.', attachments: [], streaming: false, at },
  { _tag: 'UnknownEvent', id: 'stranded/lost-event', eventType: 'opaque', data: {}, at },
  { _tag: 'Text', id: 'stranded/answer', role: 'assistant', text: 'A later answer stays in order.', attachments: [], streaming: false, at },
]
const strandedAppend: readonly ConversationItem[] = [
  { _tag: 'Notice', id: 'stranded/lost-notice', kind: 'event', text: 'An appended entry the runtime drops.', at },
  { _tag: 'Text', id: 'stranded/late-answer', role: 'assistant', text: 'The appended answer follows it.', attachments: [], streaming: false, at },
]
const strandedSecondPrompt: TextItem = { _tag: 'Text', id: 'stranded/lost-prompt', role: 'user', text: 'A prompt the runtime never adopted.', attachments: [], streaming: false, at }
const strandedSecondItems: readonly ConversationItem[] = [{ _tag: 'Text', id: 'stranded/second-answer', role: 'assistant', text: 'Its answer still renders after it.', attachments: [], streaming: false, at }]
const strandedDrop = ['stranded/lost-answer', 'stranded/lost-event', 'stranded/lost-notice', 'stranded/lost-prompt']
const settledWork = (items: readonly ConversationItem[]) => workLogTurnFromItems(items, { kindFor: () => 'read', running: false, failed: false, interrupted: false, completeHistory: true })
function StrandedItemStory({ scheme = 'dark' }: { scheme?: Scheme }) {
  const [appended, setAppended] = React.useState(false)
  const data = React.useMemo<TranscriptStoryData>(() => {
    const items = appended ? [...strandedItems, ...strandedAppend] : strandedItems
    return { sync: { _tag: 'Live', since: now }, turns: [
      { id: 'stranded', prompt: { ...strandedPrompt, role: 'user' }, items, work: settledWork(items) },
      { id: 'stranded-second', prompt: { ...strandedSecondPrompt, role: 'user' }, items: strandedSecondItems, work: settledWork(strandedSecondItems) },
    ] }
  }, [appended])
  return <main data-scheme={scheme} {...stylex.props(styles.root, ...baselineTheme, scheme === 'light' && lightTheme)}><button type="button" disabled={appended} onClick={() => setAppended(true)}>Append dropped entry</button><div {...stylex.props(styles.frame)}><RuntimeTranscript data={data} drop={strandedDrop} onRetry={() => {}} /></div></main>
}
const itemOrder = (turn: Element) => Array.from(turn.querySelectorAll('[data-item-id]'), element => element.getAttribute('data-item-id'))
export const StrandedItem: Story = { render: args => <StrandedItemStory scheme={args.scheme} />, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  const lost = await canvas.findByText('This answer never reached the runtime.')
  const lostRow = lost.closest('[data-testid="transcript-stranded"]')
  await expect(lostRow).toHaveAttribute('data-item-id', 'stranded/lost-answer')
  const eventRow = canvasElement.querySelector('[data-testid="transcript-stranded"][data-item-id="stranded/lost-event"]')
  await expect(eventRow).toHaveTextContent('Couldn\u2019t display this entry')
  const [first, second] = canvas.getAllByTestId('transcript-turn')
  await expect(itemOrder(first!)).toEqual(['stranded/prompt', 'stranded/lost-answer', 'stranded/lost-event', 'stranded/answer'])
  await expect(itemOrder(second!)).toEqual(['stranded/lost-prompt', 'stranded/second-answer'])
  await expect(within(second!).getByTestId('transcript-stranded')).toHaveTextContent('A prompt the runtime never adopted.')
  const retained = [first!, lostRow!, eventRow!, second!]
  const removed: Node[] = []
  const observer = new MutationObserver(records => { for (const record of records) for (const node of record.removedNodes) if (retained.some(element => node === element || node.contains(element))) removed.push(node) })
  observer.observe(canvasElement, { subtree: true, childList: true })
  const warn = spyOn(console, 'warn').mockImplementation(() => {})
  try {
    await userEvent.click(canvas.getByRole('button', { name: 'Append dropped entry' }))
    await waitFor(() => expect(canvasElement.querySelector('[data-testid="transcript-stranded"][data-item-id="stranded/lost-notice"]')).toHaveTextContent('An appended entry the runtime drops.'))
    await expect(itemOrder(first!)).toEqual(['stranded/prompt', 'stranded/lost-answer', 'stranded/lost-event', 'stranded/answer', 'stranded/lost-notice', 'stranded/late-answer'])
    await expect(warn).toHaveBeenCalledWith(expect.stringContaining('stranded/lost-notice'))
    observer.takeRecords().forEach(record => record.removedNodes.forEach(node => { if (retained.some(element => node === element || node.contains(element))) removed.push(node) }))
    await expect(removed).toHaveLength(0)
    const turns = canvas.getAllByTestId('transcript-turn')
    await expect(turns).toHaveLength(2)
    await expect(turns[0]).toBe(first)
    await expect(turns[1]).toBe(second)
  } finally { observer.disconnect(); warn.mockRestore() }
} }
export const StrandedItemLight: Story = { ...StrandedItem, args: { scheme: 'light' } }

const retrySend = fn<(itemId: string) => void>()
function FailedRetryStory({ scheme = 'dark' }: { scheme?: Scheme }) {
  const [state, setState] = React.useState<State>('failed-send')
  const data = React.useMemo(() => fixture(state, 'retry'), [state])
  const retry = (itemId: string) => { retrySend(itemId); setState('pending-send') }
  return <main data-scheme={scheme} {...stylex.props(styles.root, ...baselineTheme, scheme === 'light' && lightTheme)}><h2>Failed → Retry</h2><button type="button" onClick={() => setState('settled')}>Deliver echo</button><div {...stylex.props(styles.frame)}><RuntimeTranscript data={data} onOpenTool={() => {}} onRetry={() => {}} onRetrySend={retry} /></div></main>
}
export const FailedRetry: Story = { name: 'Failed → Retry', render: args => <FailedRetryStory scheme={args.scheme} />, play: async ({ canvasElement }) => {
  retrySend.mockClear()
  const canvas = within(canvasElement)
  const selector = '[data-testid="user-message"][data-item-id="retry/prompt"]'
  const row = await canvas.findByTestId('user-message')
  await expect(row).toHaveAttribute('data-send-state', 'failed')
  await userEvent.click(canvas.getByRole('button', { name: 'Retry' }))
  await expect(retrySend).toHaveBeenCalledTimes(1)
  await expect(retrySend).toHaveBeenCalledWith('retry/prompt')
  await waitFor(() => expect(row).toHaveAttribute('data-send-state', 'pending'))
  await expect(canvasElement.querySelectorAll(selector)).toHaveLength(1)
  await expect(canvasElement.querySelector(selector)).toBe(row)
  await expect(canvas.queryByRole('button', { name: 'Retry' })).toBeNull()
  await userEvent.click(canvas.getByRole('button', { name: 'Deliver echo' }))
  await waitFor(() => expect(row).toHaveAttribute('data-send-state', 'sent'))
  await expect(canvasElement.querySelectorAll(selector)).toHaveLength(1)
  await expect(canvasElement.querySelector(selector)).toBe(row)
} }
export const FailedRetryLight: Story = { ...FailedRetry, args: { scheme: 'light' } }

const promptlessData: TranscriptStoryData = {
  sync: { _tag: 'Live', since: now },
  turns: fixture('settled', 'promptless').turns.map(({ prompt, ...turn }) => turn),
}
export const MidTurnHistory: Story = { render: args => <main {...stylex.props(styles.root, ...baselineTheme, args.scheme === 'light' && lightTheme)}><RuntimeTranscript data={promptlessData} onOpenTool={() => {}} onRetry={() => {}} history={{ _tag: 'HasOlder' }} /></main>, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  await expect(await canvas.findByTestId('history-boundary')).toHaveTextContent('Earlier messages not loaded')
  await expect(await canvas.findByTestId('agent-message')).toHaveTextContent('The row projection now keeps')
  await expect(canvas.queryByTestId('user-message')).toBeNull()
  await expect(canvasElement.querySelectorAll('[data-testid="transcript-turn"]')).toHaveLength(1)
  await expect(canvas.getByTestId('history-boundary').compareDocumentPosition(canvas.getByTestId('transcript-turn')) & Node.DOCUMENT_POSITION_FOLLOWING).not.toBe(0)
} }
export const MidTurnHistoryLight: Story = { ...MidTurnHistory, args: { scheme: 'light' } }

export const ReadOnlyTools: Story = { render: args => <main {...stylex.props(styles.root, ...baselineTheme, args.scheme === 'light' && lightTheme)}><RuntimeTranscript data={cases.settled} onRetry={() => {}} /></main>, play: async ({ canvasElement }) => {
  const work = await within(canvasElement).findByTestId('work-log')
  await expect(work.querySelector(':scope > button, :scope > [role="button"]')).toBeNull()
  const rows = work.querySelectorAll('[data-tool-status]')
  await expect(rows).toHaveLength(3)
  for (const row of rows) await expect(row.querySelector('button, [role="button"], [data-row-disclosure]')).toBeNull()
  await expect(within(work).getByRole('button', { name: 'Thinking' })).toBeVisible()
  await expect(work).toHaveTextContent('export const last = visibleRows.at(-1)')
  await expect(work).toHaveTextContent('selection retained')
  await expect(work.querySelectorAll('[data-testid="tool-preview-actions"]')).toHaveLength(0)
} }
export const ReadOnlyToolsLight: Story = { ...ReadOnlyTools, args: { scheme: 'light' } }
const proseOnlyItems: readonly ConversationItem[] = [{ _tag: 'Text', id: 'prose/answer', role: 'assistant', text: 'A plain answer needs no work summary.', attachments: [], streaming: false, at }]
const proseOnlyData: TranscriptStoryData = { sync: { _tag: 'Live', since: now }, turns: [{ id: 'prose', items: proseOnlyItems, work: workLogTurnFromItems(proseOnlyItems, { kindFor: () => 'read', running: false, failed: false, interrupted: false, completeHistory: true }) }] }
export const ProseOnly: Story = { render: args => <main {...stylex.props(styles.root, ...baselineTheme, args.scheme === 'light' && lightTheme)}><RuntimeTranscript data={proseOnlyData} onRetry={() => {}} /></main>, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  await expect(await canvas.findByTestId('agent-message')).toHaveTextContent('A plain answer')
  await expect(canvas.queryByTestId('work-log')).toBeNull()
  await expect(canvas.queryByText(/Worked/)).toBeNull()
} }
export const ProseOnlyLight: Story = { ...ProseOnly, args: { scheme: 'light' } }
type FailureReason = Extract<SendState, { _tag: 'Failed' }>['reason']['_tag']
const sendFailureStory = (tag: FailureReason, copy: string, retryable: boolean): Story => ({
  render: args => {
    const base = fixture('failed-send', 'failure')
    const data: TranscriptStoryData = { ...base, turns: base.turns.map(turn => ({ ...turn, prompt: turn.prompt === undefined ? undefined : { ...turn.prompt, sendState: { _tag: 'Failed', reason: { _tag: tag }, detail: 'rpc_timeout' } } })) }
    return <main {...stylex.props(styles.root, ...baselineTheme, args.scheme === 'light' && lightTheme)}><RuntimeTranscript data={data} onRetry={() => {}} onRetrySend={() => {}} /></main>
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement)
    const failure = await canvas.findByTestId('send-failure')
    await expect(failure).toHaveTextContent(copy)
    await expect(failure).toHaveAttribute('data-send-failure-reason', tag)
    await expect(failure.textContent).not.toContain(tag)
    await expect(canvas.queryByText('rpc_timeout')).toBeNull()
    if (retryable) await expect(within(failure).getByRole('button', { name: 'Retry' })).toBeVisible()
    else await expect(within(failure).queryByRole('button', { name: 'Retry' })).toBeNull()
  },
})
export const SendRejected: Story = sendFailureStory('Rejected', 'Message was rejected', false)
export const SendUngranted: Story = sendFailureStory('Ungranted', "You don't have permission to send here", false)
export const SendInvalid: Story = sendFailureStory('Invalid', "Message couldn't be sent: it isn't valid", false)
export const SendGenericFailure: Story = sendFailureStory('Failed', "Couldn't send", true)
export const SendStaleFence: Story = sendFailureStory('StaleFence', "Couldn't send: the conversation changed", true)
export const SendSnapshotUnavailable: Story = sendFailureStory('SnapshotUnavailable', "Couldn't send: conversation not loaded yet. Nothing was sent.", true)
const semanticItems: readonly ConversationItem[] = [
  { _tag: 'Text', id: 'semantic/system', role: 'system', text: 'System context retained.', attachments: [], streaming: false, at },
  { _tag: 'Notice', id: 'semantic/notice', kind: 'redaction', text: 'Notice content retained.', at },
  { _tag: 'Notice', id: 'semantic/error-notice', kind: 'error', text: 'Error notice retained.', detail: 'Error detail retained.', at },
  { _tag: 'Message', id: 'semantic/message', messageId: 'handoff', title: 'Message handoff retained.', sender: { kind: 'harness', label: 'Host' }, at },
  { _tag: 'Message', id: 'semantic/subagent', messageId: 'subagent-result', title: 'Subagent **message** retained.', sender: { kind: 'subagent', label: 'Row reviewer' }, at },
  { _tag: 'Message', id: 'semantic/untitled', messageId: 'untitled-handoff', sender: { kind: 'harness', label: 'Host' }, at },
  { _tag: 'Event', id: 'semantic/event', kind: 'harness-message', title: 'Event title retained.', text: 'Event body retained.', sender: { kind: 'harness', label: 'Host' }, data: {}, at },
  { _tag: 'UnknownEvent', id: 'semantic/unknown', eventType: 'future.semantic.event', data: {}, at },
  { _tag: 'Status', id: 'semantic/status', status: 'completed', detail: 'Status detail retained.', at },
  { _tag: 'Usage', id: 'semantic/usage', semantics: 'response', inputTokens: 123, outputTokens: 45, at },
  { _tag: 'ToolCall', id: 'semantic/read', callId: 'semantic/read', name: 'read', input: { path: 'src/semantic.ts' }, status: 'success', result: { content: 'export const semantic = true', mediaType: 'text/plain', isError: false, at }, callSeen: true, at },
  { _tag: 'Reasoning', id: 'semantic/reasoning', text: 'Reasoning content retained.', streaming: false, at },
  { _tag: 'Text', id: 'semantic/answer', role: 'assistant', text: 'Assistant content retained.\n\n```ts\nexport const retainedContent = "The transcript keeps the prompt, tool output, reasoning, semantic entries and the later answer visible in their original order.";\n```', attachments: [], streaming: false, at },
]
const semanticData: TranscriptStoryData = {
  sync: { _tag: 'Live', since: now },
  turns: [{
    id: 'semantic',
    prompt: { _tag: 'Text', id: 'semantic/prompt', role: 'user', text: 'User content retained.', attachments: [], streaming: false, at },
    items: semanticItems,
    senderCaptions: { 'semantic/subagent': 'Review delegate' },
    work: workLogTurnFromItems(semanticItems, { kindFor: () => 'read', running: false, failed: false, interrupted: false, durationMs: 24000, completeHistory: true }),
  }],
}
export const SemanticItems: Story = { render: args => <main {...stylex.props(styles.root, ...baselineTheme, args.scheme === 'light' && lightTheme)}><RuntimeTranscript data={semanticData} onOpenTool={() => {}} onRetry={() => {}} /></main>, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  await userEvent.click(await canvas.findByRole('button', { name: /Worked for 24s/ }))
  await userEvent.click(canvas.getByRole('button', { name: 'Thinking' }))
  await expect(canvas.getByTestId('tool-detail-preview')).toBeVisible()
  await expect(canvas.getByTestId('tool-detail-preview')).toHaveTextContent('export const semantic = true')
  for (const text of ['User content retained.', 'System context retained.', 'Notice content retained.', 'Error notice retained.', 'Error detail retained.', 'Message handoff retained.', 'Message untitled-handoff', 'Event title retained.', 'Event body retained.', 'future.semantic.event', 'Status detail retained.', 'Usage · response · 123 input · 45 output', 'Reasoning content retained.', 'Assistant content retained.']) {
    await expect(canvas.getByText(text, { exact: false })).toBeVisible()
  }
  const lane = canvas.getByTestId('transcript-scroll')
  await expect(within(lane).queryByTestId('sender-avatar')).not.toBeInTheDocument()
  await expect(within(lane).queryByTestId('sender-header')).not.toBeInTheDocument()
  await expect(lane.querySelector('header')).toBeNull()
  for (const id of ['system', 'notice', 'error-notice', 'event', 'unknown', 'status', 'usage']) {
    const row = lane.querySelector(`[data-item-id="semantic/${id}"]`)
    await expect(row).toHaveAttribute('data-testid', 'transcript-message')
    await expect(row).toBeVisible()
  }
  for (const id of ['message', 'subagent', 'untitled']) {
    await expect(lane.querySelector(`[data-item-id="semantic/${id}"]`)).toHaveAttribute('data-testid', 'agent-message')
  }
  const host = within(lane.querySelector('[data-item-id="semantic/message"]') as HTMLElement).getByTestId('message-sender')
  await expect(host).toHaveTextContent('Host')
  await expect(host).toBeVisible()
  await expect(host.tagName).toBe('P')
  const subagent = lane.querySelector('[data-item-id="semantic/subagent"]') as HTMLElement
  const delegate = within(subagent).getByTestId('message-sender')
  await expect(delegate).toHaveTextContent('Review delegate')
  await expect(delegate).toBeVisible()
  await expect(delegate.tagName).toBe('P')
  await expect(subagent.querySelector('strong')).toHaveTextContent('message')
  await expect(subagent).toBeVisible()
  await expect(subagent).toHaveTextContent('Subagent message retained.')
  await expect(within(subagent).queryByText('Row reviewer')).not.toBeInTheDocument()
  const code = canvas.getByTestId('markdown-code')
  await expect(code).toHaveAttribute('tabindex', '0')
  await expect(code).toHaveAccessibleName('Code, ts')
  await expect(code.scrollWidth).toBeGreaterThan(code.clientWidth)
  within(code.closest<HTMLElement>('[data-testid="markdown-code-block"]')!).getByRole('button', { name: 'Copy code' }).focus()
  await userEvent.tab()
  await expect(code).toHaveFocus()
  const focus = getComputedStyle(code)
  await expect(focus.outlineStyle).toBe('solid')
  await expect(parseFloat(focus.outlineWidth)).toBeGreaterThan(0)
  await expect(focus.outlineColor).not.toBe('rgba(0, 0, 0, 0)')
  await expect(parseFloat(focus.outlineOffset)).toBeLessThan(0)
} }
export const SemanticItemsLight: Story = { ...SemanticItems, args: { scheme: 'light' } }
export const ReadableEmpty: Story = { args: { state: 'empty', emptyState: { title: 'No messages in this conversation', body: 'Send a message to start working with the agent.' } }, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  await expect(canvas.getByTestId('transcript-empty')).toBeInTheDocument()
  await expect(canvas.getByText('No messages in this conversation')).toBeInTheDocument()
  await expect(canvas.getByText('Send a message to start working with the agent.')).toBeInTheDocument()
  await expect(canvasElement.querySelector('[data-testid="transcript-unavailable"]')).toBeNull()
} }
export const ReadableEmptyLight: Story = { ...ReadableEmpty, args: { ...ReadableEmpty.args, scheme: 'light' } }
export const DefaultEmpty: Story = { args: { state: 'empty' }, play: async ({ canvasElement }) => {
  await expect(within(canvasElement).getByTestId('transcript-empty')).toHaveTextContent('No messages yet')
} }
export const SettledAnswerMetaLight: Story = { ...SettledAnswerMeta, args: { scheme: 'light' } }

const tokenAnswer = 'Quoted shell and diff tokens stay neutral.\n\n```bash\nprintf \'%s\\n\' "retained rows"\necho \'single quoted\' > "out file.txt"\n```\n\n```diff\n--- a/src/rows.ts\n+++ b/src/rows.ts\n@@ -1,2 +1,2 @@\n-const label = "old row"\n+const label = "new row"\n```'
const tokenItems: readonly ConversationItem[] = cases.settled.turns[0]!.items.map(item => item._tag === 'ToolCall' && item.name === 'run' ? { ...item, input: { command: 'printf \'%s\\n\' "retained rows"' }, result: { content: 'printf \'%s\\n\' "retained rows"\n+inserted line', mediaType: 'text/x-diff', isError: false, at } } : item._tag === 'Text' && item.role === 'assistant' ? { ...item, id: 'tokens/answer', text: tokenAnswer } : item)
const tokenData: TranscriptStoryData = { sync: { _tag: 'Live', since: now }, turns: [{ ...cases.settled.turns[0]!, id: 'tokens', items: tokenItems, work: workLogTurnFromItems([cases.settled.turns[0]!.prompt!, ...tokenItems], { kindFor: name => name === 'run' ? 'run' : 'read', running: false, failed: false, interrupted: false, durationMs: 24000, startedAt: at, completeHistory: true }) }] }
const noGreenData: readonly (readonly [string, TranscriptStoryData])[] = [['tokens', tokenData], ['semantic', semanticData], ...(Object.keys(cases) as State[]).map(state => [state, cases[state]] as const)]
// Green is hue 90-170° with HSL saturation above 25%; any colour function the parser cannot read fails too.
function greenFindings(root: Element): string[] {
  const findings: string[] = []
  const properties = ['color', 'background-color', 'border-top-color', 'border-right-color', 'border-bottom-color', 'border-left-color', 'outline-color', 'text-decoration-color', 'caret-color', 'fill', 'stroke', 'box-shadow', 'background-image']
  for (const element of [root, ...root.querySelectorAll('*')]) for (const pseudo of [null, '::before', '::after']) {
    const style = getComputedStyle(element, pseudo)
    for (const property of properties) {
      const value = style.getPropertyValue(property)
      if (/\b(?:oklch|oklab|lab|lch|hsla?|hwb)\(/.test(value)) { findings.push(`${property} unparsed ${value}`); continue }
      for (const match of value.matchAll(/rgba?\(([^)]*)\)|color\(srgb ([^)]*)\)/g)) {
        const parts = (match[1] ?? match[2]!).split(/[\s,/]+/).filter(Boolean).map(Number)
        const scale = match[1] === undefined ? 1 : 255
        const [red, green, blue] = parts.slice(0, 3).map(part => part / scale) as [number, number, number]
        if ((parts[3] ?? 1) === 0) continue
        const max = Math.max(red, green, blue), min = Math.min(red, green, blue), lightness = (max + min) / 2
        if (max === min) continue
        const saturation = (max - min) / (1 - Math.abs(2 * lightness - 1))
        const hue = (max === red ? ((green - blue) / (max - min) + 6) % 6 : max === green ? (blue - red) / (max - min) + 2 : (red - green) / (max - min) + 4) * 60
        if (hue >= 90 && hue <= 170 && saturation > 0.25) findings.push(`${element.tagName.toLowerCase()}${pseudo ?? ''}[${element.getAttribute('data-syntax-token') ?? element.getAttribute('data-testid') ?? ''}] ${property} ${match[0]}`)
      }
    }
  }
  return findings
}
function NoGreenStory({ scheme = 'dark' }: { scheme?: Scheme }) {
  return <main data-scheme={scheme} {...stylex.props(styles.all, ...baselineTheme, scheme === 'light' && lightTheme)}>{noGreenData.map(([name, data]) => <section key={name} data-no-green-state={name}><h2>{name}</h2><div {...stylex.props(styles.cell)}><RuntimeTranscript data={data} onOpenTool={() => {}} onRetry={() => {}} onRetrySend={() => {}} /></div></section>)}</main>
}
export const NoGreenAllStates: Story = { render: args => <NoGreenStory scheme={args.scheme} />, play: async ({ canvasElement }) => {
  await within(canvasElement).findAllByTestId('transcript-turn')
  for (let pass = 0; pass < 4; pass++) {
    const closed = [...canvasElement.querySelectorAll<HTMLElement>('[aria-expanded="false"]:not([disabled]):not([aria-disabled="true"])')]
    for (const control of closed) await userEvent.click(control)
  }
  const tokens = canvasElement.querySelector('[data-no-green-state="tokens"]')!
  await waitFor(() => expect(tokens.querySelector('[data-syntax-token~="inserted"]')).not.toBeNull())
  await expect(tokens.querySelectorAll('[data-syntax-token~="string"]').length).toBeGreaterThan(2)
  await expect(tokens.querySelector('[data-testid="tool-detail-preview"]')).not.toBeNull()
  await expect(canvasElement.querySelectorAll('[data-testid="thinking-entry"]').length).toBeGreaterThan(0)
  await expect(greenFindings(canvasElement)).toEqual([])
} }
export const NoGreenAllStatesLight: Story = { ...NoGreenAllStates, args: { scheme: 'light' } }
const styles = stylex.create({
  root: { height: '100vh', width: '100%', minWidth: 0, boxSizing: 'border-box', display: 'flex', flexDirection: 'column', backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans },
  frame: { flex: '1 1 0', minHeight: 0 }, cell: { height: '720px', display: 'flex', flexDirection: 'column' }, detail: { padding: s.lg, maxHeight: g.previewMax, overflow: 'auto', fontSize: t.metaSize },
  thread: { flex: '1 1 0', minHeight: 0, display: 'flex', flexDirection: 'column' }, composerDock: { flexShrink: 0, boxSizing: 'border-box', width: '100%', maxWidth: g.lane, marginInline: 'auto', padding: s.lg },
  all: { display: 'grid', gridTemplateColumns: 'minmax(0, 1fr) minmax(0, 1fr)', gap: s.lg, padding: s.lg, backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans },
})
