import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { expect, userEvent, waitFor, within } from 'storybook/test'
import { QuestionDock, QuestionDockGroup } from '../../../apps/fractal-web/src/web/questions/QuestionDock.tsx'
import { questionScenarios, viewedAt, submittedAt } from './assistant-ui/questions/fixtures'
import type { QuestionRequestView } from './assistant-ui/questions/model'
import { EmbraceComposer } from './assistant-ui/EmbraceComposer'
import { EmbraceRuntimeProvider } from './assistant-ui/EmbraceRuntime'
import { Transcript } from './assistant-ui/composition/Transcript'
import { baselineTheme } from './assistant-ui/neutral-theme'
import { TerminalDrawer, ThreadHeader } from './assistant-ui/composition/Shell'
import { lightTheme, type Scheme } from './assistant-ui/composition-theme'
import { surfaceVars, textVars, typeVars } from './assistant-ui/composition-tokens.stylex'

type Phase = 'pending' | 'arrival' | 'readonly' | 'answered'
interface Args { scheme: Scheme; phase: Phase; multiple: boolean; drawer: boolean; prompt?: string; empty?: boolean }
const longPrompt = 'The orchestration plan spans several worktrees and independent agents. Before continuing, choose the verification coverage that should run across the touched packages, explain how the full application suite relates to narrow checks, and account for the existing terminal session and pending release-note validation. '.repeat(4)
const headerStatusSince = viewedAt - 120_000
const requestFor = (phase: Phase): QuestionRequestView => ({
  ...questionScenarios.multi,
  ...(phase === 'readonly' ? { answerable: false, readOnlyReason: 'Answer in the agent terminal. Web answering is not available on this connection.' } : {}),
  ...(phase === 'answered' ? { state: { _tag: 'answered', by: 'You', at: submittedAt, answers: { checks: { selected: ['typecheck'], note: 'Run the narrow checks first.' } } } } : {}),
})
/** Real dock, transcript, runtime and composer. Settlement is explicitly story-only, not a live transport. */
function QuestionsStory({ scheme, phase, multiple, drawer, prompt, empty }: Args) {
  const [request, setRequest] = React.useState(() => {
    const request = requestFor(phase)
    if (empty === true) return { ...request, id: 'ask-empty', title: 'Empty ask', questions: [] }
    return prompt !== undefined ? { ...request, questions: request.questions.map(question => ({ ...question, prompt })) } : request
  })
  const [arrived, setArrived] = React.useState(phase !== 'arrival')
  const [terminalOpen, setTerminalOpen] = React.useState(drawer)
  const options = React.useMemo(() => ({ messages: [], isRunning: false, onNew: async () => {} }), [])
  return <main data-scheme={scheme} {...stylex.props(styles.root, ...baselineTheme, scheme === 'light' && lightTheme)}>
    <ThreadHeader title="Sample agent" status="waiting" statusLabel="Question pending" statusSince={headerStatusSince} freshness="live" needsMe={arrived && request.state._tag === 'pending'} panelOpen={false} drawerOpen={terminalOpen} onTogglePanel={() => {}} onToggleDrawer={() => setTerminalOpen(open => !open)} />
    <div data-testid="question-conversation-pane" style={{ flex: '1 1 0', minHeight: 0, display: 'flex', flexDirection: 'column', overflow: 'hidden' }}>
    <EmbraceRuntimeProvider options={options}>
      <div {...stylex.props(styles.history)}><Transcript title="Sample agent" turns={[]} sync={{ _tag: 'Live', since: viewedAt - 1000 }} now={viewedAt} observedAt={viewedAt} /></div>
      {arrived && <QuestionDockGroup>
        <QuestionDock request={request} now={viewedAt} onOpenTerminal={() => setTerminalOpen(true)} onSubmit={answers => setRequest(current => ({ ...current, state: { _tag: 'answered', by: 'You', at: submittedAt, answers } }))} />
        {multiple && <QuestionDock request={{ ...requestFor('readonly'), id: 'second-ask', title: 'Follow-up checks' }} now={viewedAt} />}
      </QuestionDockGroup>}
      <div data-testid="conversation-composer-dock" onInputCapture={() => setArrived(true)} {...stylex.props(styles.composer)}><EmbraceComposer variant="C1" readingColumn /></div>
    </EmbraceRuntimeProvider>
    </div>
    <TerminalDrawer open={terminalOpen} height={440} onHeight={() => {}} lines={['$ pnpm typecheck', 'Typecheck completed. The agent is waiting for your answer.']} />
  </main>
}
const meta = { title: 'Fractal/Plugins/Questions', component: QuestionsStory, parameters: { layout: 'fullscreen' }, args: { scheme: 'dark', phase: 'pending', multiple: false, drawer: false }, argTypes: { scheme: { options: ['dark', 'light'], control: 'radio' }, phase: { options: ['pending', 'arrival', 'readonly', 'answered'], control: 'radio' } } } satisfies Meta<typeof QuestionsStory>
export default meta
type Story = StoryObj<typeof meta>
export const Pending: Story = { play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  await expect(canvas.getByRole('checkbox', { name: /Typecheck/ })).toBeVisible()
  await expect(canvas.getByLabelText('Needs your attention')).toBeVisible()
  await expect(canvas.queryByText('Open question')).toBeNull()
  const dock = canvas.getByTestId('question-dock').getBoundingClientRect()
  await expect(dock.top).toBeGreaterThanOrEqual(0)
  await expect(dock.bottom).toBeLessThanOrEqual(window.innerHeight)
} }
export const Submit: Story = { play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  await userEvent.click(canvas.getByRole('checkbox', { name: /Typecheck/ }))
  await userEvent.click(canvas.getByRole('button', { name: /^Submit$/ }))
  await expect(canvas.getByText('Submitted by You.')).toBeVisible()
  await expect(canvas.queryByRole('checkbox')).toBeNull()
} }
export const ComposerFocus: Story = { args: { phase: 'arrival' }, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  const composer = canvas.getByRole('textbox')
  await userEvent.click(composer)
  await userEvent.type(composer, 'Keep this draft while the question arrives')
  await expect(composer).toHaveFocus()
  await expect(canvas.getByRole('checkbox', { name: /Typecheck/ })).toBeVisible()
} }
export const ReadOnly: Story = { args: { phase: 'readonly' }, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  await expect(canvas.getByText('Read only', { exact: true })).toBeVisible()
  await expect(canvas.queryByRole('textbox', { name: 'Note' })).toBeNull()
  await expect(canvas.queryByRole('button', { name: /^Submit$/ })).toBeNull()
  await expect(canvas.getByRole('checkbox', { name: /Typecheck/ })).toBeVisible()
  await expect(canvas.getByLabelText('Needs your attention')).toBeVisible()
  await expect(canvasElement.querySelector('[data-question-attention="true"]')).not.toBeNull()
  await userEvent.click(canvas.getByRole('button', { name: "Open the agent's terminal" }))
  await expect(canvas.getByTestId('terminal-drawer')).toBeVisible()
} }
export const Answered: Story = { args: { phase: 'answered' }, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  await expect(canvas.getByText('Submitted by You.')).toBeVisible()
  await expect(canvas.queryByRole('checkbox')).toBeNull()
  await expect(canvas.getByText(/Run the narrow checks first/)).toBeVisible()
} }
export const MultipleWithTerminalDrawer: Story = { args: { multiple: true, drawer: true }, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  const pane = canvas.getByTestId('question-conversation-pane').getBoundingClientRect()
  const group = canvas.getByTestId('question-dock-group').getBoundingClientRect()
  const composer = canvas.getByTestId('conversation-composer-dock').getBoundingClientRect()
  await expect(canvas.getAllByTestId('question-dock')).toHaveLength(2)
  await expect(group.height).toBeLessThanOrEqual(pane.height * 0.45 + 1)
  await expect(composer.bottom).toBeLessThanOrEqual(pane.bottom + 1)
  await expect(canvas.getByRole('textbox', { name: 'Message' })).toBeVisible()
} }
export const LongPrompt: Story = { args: { prompt: longPrompt, phase: 'readonly' }, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  const group = canvas.getByTestId('question-dock-group').getBoundingClientRect()
  const first = canvas.getByRole('checkbox', { name: /Typecheck/ }).closest('label')!.getBoundingClientRect()
  await expect(first.bottom).toBeLessThanOrEqual(group.bottom)
  const disclosure = canvas.getByRole('button', { name: 'Show full question' })
  await expect(disclosure).toHaveAttribute('aria-expanded', 'false')
  disclosure.focus()
  await expect(disclosure).toHaveFocus()
  await userEvent.keyboard('{Enter}')
  await expect(canvas.getByRole('button', { name: 'Show less' })).toHaveAttribute('aria-expanded', 'true')
  await userEvent.click(canvas.getByRole('button', { name: 'Show less' }))
  await expect(canvas.getByRole('button', { name: 'Show full question' })).toHaveAttribute('aria-expanded', 'false')
} }
/** Negative control: a prompt that fits in the two-line clamp renders no disclosure. */
export const ShortPrompt: Story = { play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  const prompt = canvas.getByText('Which checks should run before merge?')
  await expect(prompt.scrollHeight).toBeLessThanOrEqual(prompt.clientHeight)
  await expect(canvas.queryByRole('button', { name: 'Show full question' })).toBeNull()
} }
/** The clamp is re-measured on resize: narrowing the pane makes the same prompt overflow, widening it again removes the disclosure. */
export const PromptResize: Story = { args: { prompt: 'Which verification checks should run across the touched packages before merge?' }, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  const main = canvasElement.querySelector('main')!
  await expect(canvas.queryByRole('button', { name: 'Show full question' })).toBeNull()
  main.style.width = '240px'
  await waitFor(() => expect(canvas.getByRole('button', { name: 'Show full question' })).toBeVisible())
  main.style.width = ''
  await waitFor(() => expect(canvas.queryByRole('button', { name: 'Show full question' })).toBeNull())
} }
/** Header signals carry no native title; a kit Tooltip exposes each one on hover and on keyboard focus. */
export const HeaderSignals: Story = { play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  const page = within(canvasElement.ownerDocument.body)
  const sentence = `Question pending. Observed status since ${new Date(headerStatusSince).toISOString()}. Observation live.`
  // The kit Tooltip opens after a 350 ms delay; each wait gets an explicit budget instead of the 1 s default.
  const tooltip = async (text: string) => {
    const tip = await page.findByRole('tooltip', { name: text }, { timeout: 2000 })
    await expect(tip).toHaveTextContent(text)
    await expect(page.getAllByRole('tooltip')).toHaveLength(1)
  }
  const status = canvas.getByRole('group', { name: 'Agent status' })
  const needs = canvas.getByRole('img', { name: 'Needs your attention' })
  await expect(needs.textContent).toBe('')
  await expect(needs.querySelector('svg')).not.toBeNull()
  await expect(canvas.getByTestId('thread-header').querySelectorAll('[data-header-signal][title], [data-header-signal] [title]')).toHaveLength(0)
  // React Aria only treats a hover as a tooltip hover in pointer modality, which a pointer move sets before the pointer enters the trigger.
  await userEvent.pointer({ target: canvas.getByTestId('thread-header') })
  await userEvent.hover(needs)
  await tooltip('Needs your attention')
  await userEvent.unhover(needs)
  await waitFor(() => expect(page.queryByRole('tooltip')).toBeNull(), { timeout: 2000 })
  await userEvent.hover(status)
  await tooltip(sentence)
  await userEvent.unhover(status)
  await waitFor(() => expect(page.queryByRole('tooltip')).toBeNull(), { timeout: 2000 })
  for (let step = 0; step < 12 && canvasElement.ownerDocument.activeElement !== status; step++) await userEvent.tab()
  await expect(status).toHaveFocus()
  await tooltip(sentence)
  await userEvent.tab()
  await expect(needs).toHaveFocus()
  await tooltip('Needs your attention')
} }
/** An ask with no questions still renders its dock, and the form mounts no tablist at all (not an empty one). */
export const EmptyQuestions: Story = { args: { empty: true }, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  await expect(canvas.getByTestId('question-dock')).toBeVisible()
  const form = canvas.getByRole('region', { name: 'Empty ask' })
  await expect(form).toHaveAttribute('data-state', 'pending')
  await expect(form.querySelectorAll('[data-question-panel]')).toHaveLength(0)
  await expect(within(form).queryByRole('checkbox')).toBeNull()
  await expect(within(form).queryByRole('tablist', { hidden: true })).toBeNull()
  await expect(within(form).queryByRole('tab', { hidden: true })).toBeNull()
  await expect(within(form).queryByRole('tabpanel', { hidden: true })).toBeNull()
  await expect(canvasElement.querySelectorAll('[role="tablist"], [role="tab"], [role="tabpanel"]')).toHaveLength(0)
} }
export const AllStates: Story = { render: args => <div>{(['pending', 'readonly', 'answered'] as const).map(phase => <section key={phase} aria-label={phase}><QuestionsStory {...args} phase={phase} /></section>)}</div> }
const styles = stylex.create({
  root: { height: '100vh', display: 'flex', flexDirection: 'column', minWidth: 0, backgroundColor: surfaceVars.canvas, color: textVars.fg, fontFamily: typeVars.fontSans },
  history: { flex: '1 1 0', minHeight: 0, overflow: 'hidden' },
  composer: { flexShrink: 0, paddingBottom: 16 },
})
