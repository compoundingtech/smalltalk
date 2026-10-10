import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { useAui } from '@assistant-ui/react'
import { Button } from 'react-aria-components'
import { expect, userEvent, waitFor, within } from 'storybook/test'
import { encodeSteps, sessionByName, sessionCounts, sessionNames, stepTimestamp, worldNow, type Session, type SessionName } from '@smalltalk/st3-scenarios/sessions'
import { Transcript, type TranscriptTurn } from './assistant-ui/composition/Transcript.tsx'
import { EmbraceRuntimeProvider } from './assistant-ui/EmbraceRuntime.tsx'
import { EmbraceComposer } from './assistant-ui/EmbraceComposer.tsx'
import type { ConversationItem, TextItem } from './assistant-ui/embrace-data/model.ts'
import { workLogTurnFromItems } from './assistant-ui/taste/work-log.ts'
import { baselineTheme } from './assistant-ui/neutral-theme.ts'
import { lightTheme, type Scheme } from './assistant-ui/composition-theme.ts'
import { surfaceVars as surface, textVars as ink, borderVars as border, radiusVars as r, spaceVars as s, typeVars as t, geometryVars as g } from './assistant-ui/composition-tokens.stylex.ts'

/** Kit view fixtures, not a second wire projection. The app uses sessionEntries with its live fold.
 * Authored steps carry the existing kit view contract; wire IDs come from the prod-decoded emitter.
 */
const viewItems = (session: Session, stepCount: number): readonly ConversationItem[] => {
  const script = { ...session, steps: session.steps.slice(0, stepCount) }
  const emitted = encodeSteps(script)
  const cancelled = script.steps.some(step => step.kind === 'status' && step.status === 'cancelled')
  return script.steps.map((step, index): ConversationItem => {
    const id = emitted[index]![step.kind === 'user' ? 1 : 0]!.id
    const at = stepTimestamp(session, step.t)
    switch (step.kind) {
      case 'user': case 'say': return { _tag: 'Text', id, role: step.kind === 'user' ? 'user' : 'assistant', text: step.text, attachments: [], streaming: step.kind === 'say' && step.final === false, at }
      case 'think': return { _tag: 'Reasoning', id, text: step.text, durationMs: step.ms, streaming: false, at }
      case 'tool': return { _tag: 'ToolCall', id, callId: step.id, name: step.name, input: step.input, status: step.output === undefined ? cancelled ? 'interrupted' : 'running' : step.isError === true ? 'error' : 'success', callSeen: true, at, ...(step.output === undefined ? {} : { result: { content: step.output, mediaType: 'text/plain', isError: step.isError === true, at: stepTimestamp(session, step.end ?? step.t) } }) }
      case 'status': return { _tag: 'Status', id, status: step.status, detail: step.detail, at }
      case 'mail': return { _tag: 'Message', id, messageId: step.id, from: step.from, to: step.to, title: step.title, at }
    }
  })
}

const viewTurn = (session: Session, count: number): readonly TranscriptTurn[] => {
  const items = viewItems(session, count)
  if (items.length === 0) return []
  const prompt = items.find((item): item is TextItem & { readonly role: 'user' } => item._tag === 'Text' && item.role === 'user')
  const status = items.filter(item => item._tag === 'Status').at(-1)
  const interrupted = status?._tag === 'Status' && status.status === 'cancelled'
  const running = status?._tag === 'Status' && status.status === 'running'
  const visible = items.filter(item => item !== prompt)
  return [{ id: session.id, prompt, items: visible, work: workLogTurnFromItems(items, { kindFor: name => name === 'bash' ? 'run' : name === 'read' ? 'read' : 'edit', running, failed: false, interrupted, completeHistory: true, startedAt: stepTimestamp(session, 0), durationMs: sessionCounts({ ...session, steps: session.steps.slice(0, count) }).durationMs }) }]
}

const styles = stylex.create({
  root: { height: '100vh', width: '100%', minWidth: 0, boxSizing: 'border-box', display: 'flex', flexDirection: 'column', backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans },
  header: { display: 'flex', gap: s.lg, alignItems: 'baseline', paddingInline: s.lg, paddingBlock: s.md, fontSize: t.metaSize, lineHeight: t.metaLeading },
  heading: { margin: 0, fontSize: t.bodySize, lineHeight: t.bodyLeading },
  meta: { margin: 0, color: ink.fgMuted },
  frame: { flex: '1 1 0', minHeight: 0, display: 'flex', flexDirection: 'column' },
  dock: { flexShrink: 0, boxSizing: 'border-box', width: '100%', maxWidth: g.lane, marginInline: 'auto', padding: s.lg },
  action: { alignSelf: 'center', marginBlock: s.md, backgroundColor: surface.controlFill, color: ink.fg, paddingBlock: s.xs, paddingInline: s.md, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.controlBorder, borderRadius: r.control, fontFamily: t.fontSans, fontSize: t.metaSize, cursor: 'pointer' },
  examples: { display: 'grid', gridTemplateColumns: 'minmax(0, 1fr) minmax(0, 1fr)', gap: s.lg, padding: s.lg, backgroundColor: surface.canvas },
  cell: { height: '720px', display: 'flex', flexDirection: 'column' },
  embedded: { height: '100%' },
})

/** Host restore path: the saved draft is written back into the real composer runtime. */
function RestoreDraft({ text }: { readonly text: string }) {
  const aui = useAui()
  React.useEffect(() => { aui.composer.setText(text) }, [aui, text])
  return null
}

function SessionStory({ name = 'short-success', scheme = 'dark', embedded = false }: { name?: SessionName; scheme?: Scheme; embedded?: boolean }) {
  const session = sessionByName(name)
  const [count, setCount] = React.useState(session.initialStepCount ?? session.steps.length)
  const turns = React.useMemo(() => viewTurn(session, count), [session, count])
  const messages = React.useMemo(() => turns.flatMap(turn => turn.prompt === undefined ? turn.items : [turn.prompt, ...turn.items]), [turns])
  const offline = name === 'offline-retry' && count !== session.steps.length
  const options = React.useMemo(() => ({ messages, isRunning: false, onNew: async () => {} }), [messages])
  const counts = sessionCounts({ ...session, steps: session.steps.slice(0, count) })
  return <section aria-label={`${session.title} session`} {...stylex.props(styles.root, embedded && styles.embedded, ...baselineTheme, scheme === 'light' && lightTheme)}>
    <header {...stylex.props(styles.header)}><h2 {...stylex.props(styles.heading)}>{session.title}</h2><p {...stylex.props(styles.meta)}>{counts.entries} entries · {counts.toolCalls} tool calls · {counts.durationMs / 1000}s observed</p></header>
    <EmbraceRuntimeProvider options={options}>
      <div {...stylex.props(styles.frame)}>
        <Transcript title={session.title} turns={turns} now={worldNow} observedAt={worldNow - 3000} history={{ _tag: 'Complete' }}
          sync={offline ? { _tag: 'Stale', reason: { _tag: 'Reconnecting', attempt: 1, nextAt: worldNow, issue: 'Offline connection' }, lastLiveAt: Date.parse(stepTimestamp(session, 17)) } : { _tag: 'Live', since: worldNow }}
          onRetrySync={() => setCount(session.steps.length)} emptyState={{ title: 'No messages yet', body: 'Receive the first result to start this fictional session.' }} landmarkContext={session.title} />
      </div>
      {session.draft !== undefined && <div {...stylex.props(styles.dock)}><RestoreDraft text={session.draft} /><EmbraceComposer variant="C1" plainText sendDisabled={offline || name === 'interrupted-draft'} /></div>}
    </EmbraceRuntimeProvider>
    {offline && <Button onPress={() => setCount(session.steps.length)} {...stylex.props(styles.action)}>Retry connection</Button>}
    {name === 'first-result' && count === 0 && <Button onPress={() => setCount(session.steps.length)} {...stylex.props(styles.action)}>Receive first result</Button>}
  </section>
}

const witnesses: Record<SessionName, string> = {
  'short-success': 'Inventory heading corrected. No behavior changed.',
  'flaky-test': 'Expiry rerun passed all 20 runs after the clock repair. The original assertion is unchanged.',
  'multi-file-refactor': 'Review handoff ready: three files changed, parsing isolated, totals retained.',
  'interrupted-draft': 'Migration draft: validate quantities, preserve the original file, then write the version marker.',
  'offline-retry': 'Stock refreshed after retry: 21 units at revision 5.',
  'first-result': 'First result: this project tracks fictional stock quantities.',
  'long-debug': 'Debug conclusion: the inclusive cursor duplicated boundary rows.',
  'waiting-queued': 'Waiting for your rounding policy: retain fractions or round down?',
}

/** The same assertion must reject an off-DOM planted mutation; no failed play is published. */
const assertWitness = async (root: HTMLElement, name: SessionName): Promise<void> => {
  const answers = root.querySelectorAll('[data-testid="agent-message"]')
  if (![...answers].some(answer => answer.textContent?.includes(witnesses[name]))) {
    throw new Error(`Missing ${name} transcript witness`)
  }
}
const proveWitness = async (root: HTMLElement, name: SessionName): Promise<void> => {
  await expect(assertWitness(root, name)).resolves.toBeUndefined()
  const planted = root.cloneNode(true)
  if (!(planted instanceof HTMLElement)) throw new Error('Expected an HTML story control')
  planted.querySelectorAll('[data-testid="agent-message"]').forEach(answer => answer.remove())
  await expect(assertWitness(planted, name)).rejects.toThrow()
}
const exerciseSession = async (canvasElement: HTMLElement, name: SessionName): Promise<void> => {
  const canvas = within(canvasElement)
  if (name === 'first-result') {
    await expect(canvas.queryByTestId('agent-message')).toBeNull()
    await userEvent.click(canvas.getByRole('button', { name: 'Receive first result' }))
  }
  if (name === 'offline-retry') {
    await expect(canvas.getByText('Cached stock is retained while the connection is offline. This result may be stale.')).toBeVisible()
    await waitFor(() => expect(canvas.getByRole('textbox', { name: 'Message' })).toHaveValue(sessionByName(name).draft))
    await userEvent.click(canvas.getByRole('button', { name: 'Retry connection' }))
  }
  await proveWitness(canvasElement, name)
  if (name === 'interrupted-draft' || name === 'offline-retry') {
    await waitFor(() => expect(canvas.getByRole('textbox', { name: 'Message' })).toHaveValue(sessionByName(name).draft))
  }
  if (name === 'long-debug') {
    await expect(canvas.getByRole('table')).toHaveTextContent('Repeated boundary')
    canvas.getByTestId('transcript-scroll').scrollTo({ top: 0 })
    await expect(await canvas.findByText(/^Worked for/)).toBeInTheDocument()
    await expect(await canvas.findAllByText(/No output/)).toHaveLength(2)
  }
}
const playSession = (name: SessionName): NonNullable<Story['play']> => async ({ canvasElement }) => exerciseSession(canvasElement, name)

const meta = { title: 'Fractal/Kit/Sessions', component: SessionStory, parameters: { layout: 'fullscreen' }, args: { name: 'short-success', scheme: 'dark' }, argTypes: { name: { options: sessionNames, control: 'select' }, scheme: { options: ['dark', 'light'], control: 'radio' } } } satisfies Meta<typeof SessionStory>
export default meta
type Story = StoryObj<typeof meta>
export const ShortSuccess: Story = { args: { name: 'short-success' }, play: playSession('short-success') }
export const FlakyTest: Story = { args: { name: 'flaky-test' }, play: playSession('flaky-test') }
export const MultiFileRefactor: Story = { args: { name: 'multi-file-refactor' }, play: playSession('multi-file-refactor') }
export const InterruptedDraft: Story = { args: { name: 'interrupted-draft' }, play: playSession('interrupted-draft') }
export const OfflineRetry: Story = { args: { name: 'offline-retry' }, play: playSession('offline-retry') }
export const FirstResult: Story = { args: { name: 'first-result' }, play: playSession('first-result') }
export const LongDebug: Story = { args: { name: 'long-debug' }, play: playSession('long-debug') }
export const WaitingQueued: Story = { args: { name: 'waiting-queued' }, play: playSession('waiting-queued') }
export const AllStates: Story = {
  render: ({ scheme }) => <div {...stylex.props(styles.examples)}>{sessionNames.map(name => <div key={name} {...stylex.props(styles.cell)}><SessionStory name={name} scheme={scheme} embedded /></div>)}</div>,
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement)
    await expect(canvas.getAllByRole('region', { name: / session$/ })).toHaveLength(8)
    for (const name of sessionNames) {
      const region = canvas.getByRole('region', { name: `${sessionByName(name).title} session` })
      await exerciseSession(region, name)
    }
  },
}
