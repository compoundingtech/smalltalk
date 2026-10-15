import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { expect, fn, userEvent, waitFor, within } from 'storybook/test'
import { ThreadHeader } from './assistant-ui/composition/Shell'
import { Transcript, type TranscriptTurn } from './assistant-ui/composition/Transcript'
import { EmbraceComposer } from './assistant-ui/EmbraceComposer'
import { EmbraceRuntimeProvider } from './assistant-ui/EmbraceRuntime'
import type { ConversationItem, TextItem } from './assistant-ui/embrace-data/model'
import { workLogTurnFromItems } from './assistant-ui/taste/work-log'
import { baselineTheme } from './assistant-ui/neutral-theme'
import { darkTheme, liveAccentTheme, liveComposerDarkTheme } from './assistant-ui/embrace-theme'
import { lightTheme, type Scheme } from './assistant-ui/composition-theme'
import { surfaceVars as surface, textVars as ink, spaceVars as s, typeVars as t } from './assistant-ui/composition-tokens.stylex'

const now = Date.parse('2026-01-15T12:04:12Z')
const startedAt = '2026-01-15T12:00:00Z'
const at = (seconds: number) => new Date(Date.parse(startedAt) + seconds * 1000).toISOString()

const prompt: TextItem & { role: 'user' } = { _tag: 'Text', id: 'run/prompt', role: 'user', text: 'Rebase the row-projection branch on main and rerun the focused checks.', attachments: [], streaming: false, at: startedAt, sender: { kind: 'human', label: 'Operator' } }
const done = (id: string, name: string, input: Record<string, string>, content: string, start: number, end: number): ConversationItem => ({ _tag: 'ToolCall', id: `run/${id}`, callId: `run/${id}`, name, input, status: 'success', result: { content, mediaType: 'text/plain', isError: false, at: at(end) }, callSeen: true, at: at(start) })
const settled: readonly ConversationItem[] = [
  done('fetch', 'run', { command: 'git fetch origin main' }, 'From origin\n * branch main -> FETCH_HEAD', 2, 4),
  done('rebase', 'run', { command: 'git rebase origin/main' }, 'Successfully rebased and updated refs/heads/row-projection.', 5, 9),
  done('read', 'read', { path: 'src/rows.ts' }, 'export const visibleRows = rows.filter(isVisible)', 10, 11),
]
const note: TextItem = { _tag: 'Text', id: 'run/note', role: 'assistant', text: 'The rebase is clean. Running the focused row checks now; they take a few minutes.', attachments: [], streaming: false, at: at(12), sender: { kind: 'agent', label: 'Assistant' } }
const check = (status: 'running' | 'interrupted'): ConversationItem => ({ _tag: 'ToolCall', id: 'run/check', callId: 'run/check', name: 'run', input: { command: 'pnpm vitest run src/rows --reporter dot' }, status, ...(status === 'interrupted' ? { result: { content: 'Stopped before the checks completed.', mediaType: 'text/plain', isError: false, at: at(250) } } : {}), callSeen: true, at: at(14) })
const turnFor = (running: boolean): TranscriptTurn => {
  const items = [...settled, note, check(running ? 'running' : 'interrupted')]
  return { id: 'run', prompt, items, work: workLogTurnFromItems([prompt, ...items], { kindFor: name => name === 'run' ? 'run' : 'read', running, failed: false, interrupted: !running, startedAt, completeHistory: true }) }
}
const sync = { _tag: 'Live', since: now - 5000 } as const
const refusal = 'That agent has no terminal to stop its turn through.'

/**
 * The thread as the app composes it: ThreadHeader, Transcript and the C1 composer in one runtime.
 * While a run can be stopped, Stop sits beside Send and Escape in the composer stops too. Stop asks
 * the runtime's `onCancel`; here the run ends 1.5 s later, or the host refuses with a reason.
 */
function RunStopStory({ scheme, refuse, onStopRequest }: { scheme: Scheme; refuse: boolean; onStopRequest: () => void }) {
  const [running, setRunning] = React.useState(true)
  const [stopRefusal, setStopRefusal] = React.useState<string>()
  const turn = React.useMemo(() => turnFor(running), [running])
  const messages = React.useMemo(() => [turn.prompt!, ...turn.items], [turn])
  const onCancel = React.useCallback(async () => {
    onStopRequest()
    setStopRefusal(undefined)
    window.setTimeout(() => refuse ? setStopRefusal(refusal) : setRunning(false), 1500)
  }, [refuse, onStopRequest])
  const options = React.useMemo(() => ({ messages, isRunning: running, onNew: async () => {}, onCancel }), [messages, running, onCancel])
  return <main aria-label="Thread" data-scheme={scheme} {...stylex.props(styles.root, ...baselineTheme, scheme === 'dark' && darkTheme, scheme === 'dark' && liveComposerDarkTheme, liveAccentTheme, scheme === 'light' && lightTheme)}>
    <EmbraceRuntimeProvider options={options}>
      <ThreadHeader
        folder="workstation-a" title="Row projection" status={running ? 'working' : 'idle'} statusLabel={running ? 'working' : 'idle'} statusSince={Date.parse(startedAt)} freshness="live" now={now}
        terminalAvailable panelOpen={false} drawerOpen={false} onTogglePanel={() => {}} onToggleDrawer={() => {}}
      />
      <div {...stylex.props(styles.transcript)}><Transcript viewportKey="run" title="Row projection" turns={[turn]} sync={sync} now={now} observedAt={now - 3000} onOpenTool={() => {}} /></div>
      <div data-testid="story-composer-dock" {...stylex.props(styles.dock)}><EmbraceComposer variant="C1" readingColumn {...(stopRefusal === undefined ? {} : { stopRefusal })} /></div>
    </EmbraceRuntimeProvider>
  </main>
}

const meta = {
  title: 'Fractal UI/Run stop',
  component: RunStopStory,
  parameters: { layout: 'fullscreen' },
  args: { scheme: 'dark', refuse: false, onStopRequest: fn() },
  argTypes: { scheme: { options: ['dark', 'light'], control: 'radio' }, refuse: { control: 'boolean' } },
} satisfies Meta<typeof RunStopStory>
export default meta
type Story = StoryObj<typeof meta>

const stop = (canvasElement: HTMLElement) => canvasElement.querySelector<HTMLButtonElement>('[data-testid="run-stop"]')
/** Pressing Stop (or Escape) asks the host once and holds "Stopping…" until the runtime ends the run; then Stop leaves and the call reads interrupted. */
const stopsOnce = (via: 'press' | 'escape') => async ({ canvasElement, args }: { canvasElement: HTMLElement; args: { onStopRequest: () => void } }) => {
  const canvas = within(canvasElement)
  await waitFor(() => expect(stop(canvasElement)).toHaveAttribute('aria-label', 'Stop run'))
  if (via === 'press') await userEvent.click(stop(canvasElement)!)
  else { await userEvent.click(canvas.getByRole('textbox', { name: 'Message' })); await userEvent.keyboard('{Escape}') }
  await waitFor(() => expect(stop(canvasElement)).toHaveAttribute('aria-label', 'Stopping run'))
  await expect(stop(canvasElement)).toBeDisabled()
  await waitFor(() => expect(stop(canvasElement)).toBeNull(), { timeout: 4000 })
  await expect(canvasElement.querySelector('[data-tool-status="interrupted"]')).not.toBeNull()
  await expect(args.onStopRequest).toHaveBeenCalledTimes(1)
}

/** While the run is active Stop sits left of Send; Send keeps its one meaning. Escape is announced, not drawn. */
export const Running: Story = { play: async ({ canvasElement }) => {
  await waitFor(() => expect(stop(canvasElement)).toHaveAttribute('aria-label', 'Stop run'))
  await expect(stop(canvasElement)).toHaveAttribute('aria-keyshortcuts', 'Escape')
  const send = within(canvasElement).getByRole('button', { name: 'Send' })
  await expect(send).toBeVisible()
  await expect(stop(canvasElement)!.compareDocumentPosition(send) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy()
  // Typing a follow-up keeps Stop reachable.
  await userEvent.type(within(canvasElement).getByRole('textbox', { name: 'Message' }), 'also run lint')
  await expect(stop(canvasElement)).toHaveAttribute('aria-label', 'Stop run')
} }
export const RunningLight: Story = { ...Running, args: { scheme: 'light' } }
export const StopsOnPress: Story = { play: stopsOnce('press') }
export const StopsOnEscape: Story = { play: stopsOnce('escape') }
/** Escape that dismisses an IME composition belongs to the IME: nothing asks to stop the run. Once composition ends, Escape stops it. */
export const EscapeDuringCompositionKeepsRunning: Story = { play: async ({ canvasElement, args }) => {
  await waitFor(() => expect(stop(canvasElement)).toHaveAttribute('aria-label', 'Stop run'))
  const field = within(canvasElement).getByRole('textbox', { name: 'Message' })
  await userEvent.click(field)
  field.dispatchEvent(new CompositionEvent('compositionstart', { bubbles: true, data: '' }))
  field.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', code: 'Escape', bubbles: true, cancelable: true, isComposing: true }))
  field.dispatchEvent(new CompositionEvent('compositionend', { bubbles: true, data: '' }))
  // keyCode 229 alone (Safari reports IME keys this way without isComposing) is the IME's too.
  field.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', code: 'Escape', bubbles: true, cancelable: true, keyCode: 229 }))
  // Barrier: a later keystroke has rendered, so any stop those Escapes asked for would have reached the host.
  await userEvent.keyboard('x')
  await waitFor(() => expect(field).toHaveValue('x'))
  await expect(args.onStopRequest).not.toHaveBeenCalled()
  await expect(stop(canvasElement)).toHaveAttribute('aria-label', 'Stop run')
  await expect(stop(canvasElement)).toBeEnabled()
  // Positive control on the same path: once composition ended, Escape stops the run exactly once.
  field.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', code: 'Escape', bubbles: true, cancelable: true }))
  await waitFor(() => expect(args.onStopRequest).toHaveBeenCalledTimes(1))
  await waitFor(() => expect(stop(canvasElement)).toHaveAttribute('aria-label', 'Stopping run'))
  await waitFor(() => expect(stop(canvasElement)).toBeNull(), { timeout: 4000 })
} }
/** The host could not stop the run: its reason shows beside Stop, which can be pressed again. */
export const StopRefused: Story = { args: { refuse: true }, play: async ({ canvasElement }) => {
  await waitFor(() => expect(stop(canvasElement)).toHaveAttribute('aria-label', 'Stop run'))
  await userEvent.click(stop(canvasElement)!)
  await waitFor(() => expect(stop(canvasElement)).toHaveAttribute('aria-label', 'Stopping run'))
  await waitFor(() => expect(stop(canvasElement)).toHaveAttribute('aria-label', 'Stop run'), { timeout: 4000 })
  await expect(stop(canvasElement)).toBeEnabled()
  await expect(canvasElement.querySelector('[data-testid="composer-footer"] [role="status"]')).toHaveAttribute('title', refusal)
} }
export const StopRefusedLight: Story = { ...StopRefused, args: { refuse: true, scheme: 'light' } }

const styles = stylex.create({
  root: { height: '100vh', width: '100%', boxSizing: 'border-box', display: 'flex', flexDirection: 'column', backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans },
  transcript: { flex: '1 1 0', minHeight: 0, display: 'flex', flexDirection: 'column' },
  dock: { flexShrink: 0, paddingBottom: s.lg },
})
