import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { expect, userEvent, within } from 'storybook/test'
import { Button } from 'react-aria-components'
import { Workbench } from './assistant-ui/workbench/Workbench'
import type { DiffRevealRequest } from './assistant-ui/composition/DiffPanel'
import type { WorkLogCall } from './assistant-ui/taste/work-log'
import { AgentDragRow, DragToSplitFixture } from './assistant-ui/workbench/DragToSplitFixture'
import { openAgentAtPath, type AgentPlacement } from './assistant-ui/workbench/agent-drag'
import { findGroupPath, group, split, parsePaneKey, type WorkbenchLayout, type WorkbenchResources } from './assistant-ui/workbench/workbench-model'
import { readStoredLayout, storeLayout } from './assistant-ui/workbench/workbench-state'
import { decidedAppearance, fixtureNow, onePaneLayout, twoPaneLayout, nestedLayout, useWorkbenchFixture } from './assistant-ui/workbench/workbench-fixtures'
import { TerminalFixturePanel } from './assistant-ui/workbench/TerminalFixturePanel'
import { useTerminalFixture } from './assistant-ui/workbench/terminal-fixtures'
import { baselineTheme } from './assistant-ui/neutral-theme'
import { lightTheme, type Scheme } from './assistant-ui/composition-theme'
import { surfaceVars as surface, textVars as text, borderVars as border, accentVars as accent, typeVars as t, geometryVars as g, spaceVars as s } from './assistant-ui/composition-tokens.stylex'

type State = 'one' | 'two' | 'three' | 'dragging' | 'nested' | 'tabs'
type ThreadState = 'live' | 'empty' | 'loading' | 'stale' | 'failed' | 'unavailable'
const tabLayout = group([{ uri: 'agent:worker-1' }, { uri: 'agent:worker-2' }])
const fixtureLayout = (state: State): WorkbenchLayout => state === 'one' ? onePaneLayout : state === 'nested' ? nestedLayout : state === 'tabs' ? tabLayout : twoPaneLayout
function WorkbenchStory({ scheme = 'dark', state = 'two', threadState = 'live', seed = 'running', persist = true, workspaceId = `kit-b-${scheme}-${state}-${seed}-${threadState}` }: { scheme?: Scheme; state?: State; threadState?: ThreadState; seed?: 'running' | 'idle'; persist?: boolean; workspaceId?: string }) {
  const fixture = useWorkbenchFixture(seed)
  const initial = fixtureLayout(state)
  const [layout, setLayout] = React.useState(() => persist ? readStoredLayout(workspaceId, initial) : initial)
  const [epoch, setEpoch] = React.useState(0)
  const [focused, setFocused] = React.useState('agent:worker-1')
  const [terminalOpen, setTerminalOpen] = React.useState(state === 'three')
  const [terminalRef, setTerminalRef] = React.useState<string>()
  const terminal = useTerminalFixture(fixture.resources.terminals)
  const [revealRequest, setRevealRequest] = React.useState<DiffRevealRequest>()
  // The fixture host explicitly chooses the file for its one known tool call; the kit reads no item path.
  const onOpenTool = React.useCallback((_call: WorkLogCall) => setRevealRequest(previous => ({ path: 'sample/rows.ts', sequence: (previous?.sequence ?? 0) + 1 })), [])
  const hostResources = React.useMemo<WorkbenchResources>(() => ({
    ...fixture.resources,
    threads: new Map([...fixture.resources.threads].map(([uri, thread]) => [uri, uri === 'agent:worker-1' ? { ...thread, transcript: { ...thread.transcript, onOpenTool } } : thread])),
  }), [fixture.resources, onOpenTool])
  const resources = React.useMemo<WorkbenchResources>(() => {
    if (threadState === 'live') return hostResources
    const threads = new Map([...hostResources.threads].map(([uri, thread]) => {
      const noMessages = threadState === 'empty' || threadState === 'loading'
      return [uri, { ...thread, runtime: { ...thread.runtime, ...(noMessages ? { messages: [] } : {}), isRunning: false }, transcript: { ...thread.transcript, ...(noMessages ? { turns: [] } : {}), sync: threadState === 'loading' ? { _tag: 'Connecting' as const, attempt: 1, since: fixtureNow } : threadState === 'stale' ? { _tag: 'Stale' as const, reason: { _tag: 'Reconnecting' as const, attempt: 2, nextAt: fixtureNow + 5000, issue: 'Fixture connection interrupted' }, lastLiveAt: fixtureNow - 10000 } : threadState === 'failed' ? { _tag: 'Failed' as const, cause: { _tag: 'Server' as const, code: 'FIXTURE', message: 'Fixture synchronization failed' } } : { _tag: 'Live' as const, since: fixtureNow }, ...(threadState === 'unavailable' ? { availability: { _tag: 'Unavailable' as const, reason: 'This conversation is unavailable on this device.' } } : {}) } } ] as const
    }))
    return { ...hostResources, threads }
  }, [hostResources, threadState])
  const open = (placement: AgentPlacement) => {
    const path = findGroupPath(layout, focused) ?? findGroupPath(layout)
    if (path !== undefined) { const next = openAgentAtPath(layout, path, 'agent:worker-2', placement); storeLayout(workspaceId, next); setLayout(next); setFocused('agent:worker-2') }
  }
  const openTerminal = (ref?: string) => { setTerminalRef(ref); setTerminalOpen(true) }
  return <section aria-label={`Workbench, ${state}, ${threadState}`} data-testid="workbench-story" data-state={state} data-layout={JSON.stringify(layout)} data-workspace={workspaceId} {...stylex.props(styles.canvas, ...baselineTheme, scheme === 'light' && lightTheme)}>
    <div role="toolbar" aria-label="Workbench fixture actions" {...stylex.props(styles.toolbar)}><AgentDragRow onOpen={open} /><AgentDragRow agentKey="terminal/worker-1" title="Checks terminal" onOpen={() => openTerminal('terminal/worker-1')} /><Button onPress={() => { storeLayout(workspaceId, initial); setLayout(initial); setFocused('agent:worker-1'); setRevealRequest(undefined); setEpoch(value => value + 1) }} {...stylex.props(styles.button)}>Reset layout</Button><span>W3 · H1 · P3 · D2 · deterministic host</span></div>
    <div {...stylex.props(styles.workspace)}><Workbench key={epoch} layout={layout} resources={resources} workspaceId={workspaceId} scheme={scheme} appearance={decidedAppearance} landmarkContext={state} previewPlacement={state === 'dragging' ? 'right' : undefined} focusedPaneKey={focused} onPaneSelect={setFocused} onLayoutChange={setLayout} describePane={fixture.describePane} revealRequest={revealRequest} onOpenTerminal={openTerminal} /></div>
    {terminalOpen && <TerminalFixturePanel frame={terminal.frame} selectedRef={terminalRef} onSelect={setTerminalRef} onHide={() => setTerminalOpen(false)} onAdd={() => { const ref = terminal.add(); if (ref !== undefined) setTerminalRef(ref) }} onKill={terminal.kill} />}
  </section>
}
const meta = {
  title: 'Fractal UI/Workbench', component: WorkbenchStory, parameters: { layout: 'fullscreen' }, args: { scheme: 'dark', state: 'two', threadState: 'live', seed: 'running' },
  argTypes: { scheme: { options: ['dark', 'light'], control: 'radio' }, state: { options: ['one', 'two', 'three', 'dragging', 'nested', 'tabs'], control: 'select' }, threadState: { options: ['live', 'empty', 'loading', 'stale', 'failed', 'unavailable'], control: 'select' }, seed: { options: ['running', 'idle'], control: 'radio' }, persist: { table: { disable: true } }, workspaceId: { table: { disable: true } } },
  render: (args, { id }) => <main aria-label="Workbench"><WorkbenchStory key={`${id}-${args.scheme}-${args.state}-${args.seed}-${args.threadState}`} {...args} workspaceId={`kit-b-${id}-${args.scheme}-${args.state}${args.seed === 'idle' ? '-idle' : ''}-${args.threadState}`} /></main>,
} satisfies Meta<typeof WorkbenchStory>
export default meta
type Story = StoryObj<typeof meta>
const settle = () => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve())))
export const Dark: Story = {}
export const Light: Story = { args: { scheme: 'light' } }
export const OnePane: Story = { args: { state: 'one' } }
/** The exact reviewed idle sample seed keeps plain Enter's immediate-send case independent of running policy. */
export const IdleSample: Story = { args: { state: 'one', seed: 'idle' }, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  const input = canvas.getByTestId('composer-input')
  await userEvent.clear(input)
  await userEvent.type(input, 'Send from the idle sample fixture{Enter}')
  await canvas.findByText('Send from the idle sample fixture')
  await expect(canvas.getByTestId('pane-header')).toHaveTextContent('Done')
  await expect(canvas.queryByText(/will send after run/)).toBeNull()
} }
/** Plain Enter queues during a run; a modifier explicitly steers without draining that queue. */
export const RunningQueueAndSteer: Story = { args: { state: 'one', seed: 'running' }, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  const input = canvas.getByTestId('composer-input')
  await userEvent.clear(input)
  await userEvent.type(input, 'Keep this message queued{Enter}')
  await expect(canvas.getByText('1 message will send after run')).toBeVisible()
  await expect(canvas.queryByText('Keep this message queued')).toBeNull()
  await userEvent.type(input, 'Steer the active run')
  await userEvent.keyboard('{Control>}{Enter}{/Control}')
  await canvas.findByText('Steer the active run')
  await expect(canvas.getByText('1 message will send after run')).toBeVisible()
} }
export const TerminalOpen: Story = { args: { state: 'three' } }
export const Dragging: Story = { args: { state: 'dragging' } }
export const NestedRatios: Story = { args: { state: 'nested' } }
export const ThreadTabs: Story = { args: { state: 'tabs' } }
export const Empty: Story = { args: { state: 'one', threadState: 'empty' } }
export const Loading: Story = { args: { state: 'one', threadState: 'loading' } }
export const Stale: Story = { args: { state: 'one', threadState: 'stale' } }
export const Failed: Story = { args: { state: 'one', threadState: 'failed' } }
export const Unavailable: Story = { args: { state: 'one', threadState: 'unavailable' } }
export const SplitBelow: Story = { render: function Render(args) {
  const fixture = useWorkbenchFixture()
  return <main {...stylex.props(styles.canvas, ...baselineTheme, args.scheme === 'light' && lightTheme)}><Workbench layout={split('below', onePaneLayout, group([{ uri: 'diff:sample/rows.ts', form: 'unified' }]))} resources={fixture.resources} appearance={decidedAppearance} scheme={args.scheme} workspaceId="kit-b-below" /></main>
} }
export const DragToSplit: Story = { render: args => <div {...stylex.props(...baselineTheme)}><DragToSplitFixture scheme={args.scheme} /></div> }
export const DragToSplitSingleGroup: Story = { render: args => <div {...stylex.props(...baselineTheme)}><DragToSplitFixture scheme={args.scheme} groups={1} /></div> }
export const RendererOverride: Story = { render: function Render(args) {
  const fixture = useWorkbenchFixture()
  return <main {...stylex.props(styles.canvas, ...baselineTheme, args.scheme === 'light' && lightTheme)}><Workbench layout={group([{ uri: 'custom:editor' }])} resources={fixture.resources} scheme={args.scheme} workspaceId="kit-b-override" renderPane={() => <label {...stylex.props(styles.override)}>Host-owned editor<textarea aria-label="Host editor" defaultValue="An application-owned pane rendered through renderPane." /></label>} /></main>
} }

/** Effect 3 optional fields also permit an explicitly present undefined value. */
export const OptionalSnapshotFields: Story = { args: { state: 'one' }, play: async () => {
  const workspace = 'kit-b-optional-codec'
  const layout: WorkbenchLayout = {
    kind: 'split', split: 'right', ratio: undefined,
    children: [group([parsePaneKey('agent:worker-1 form=default')]), group([parsePaneKey('agent:worker-2 view=secondary')])],
  }
  localStorage.removeItem(`workbench.${workspace}.snapshot`)
  storeLayout(workspace, layout)
  const snapshot = localStorage.getItem(`workbench.${workspace}.snapshot`)
  await expect(snapshot).not.toBeNull()
  await expect(JSON.parse(snapshot!)).toEqual(JSON.parse(JSON.stringify(layout)))
  await expect(readStoredLayout(workspace, onePaneLayout)).toEqual(JSON.parse(JSON.stringify(layout)))
  localStorage.removeItem(`workbench.${workspace}.snapshot`)
} }

/** Closing a nested pane promotes its neighbour without losing ratios or the exact host identity. */
export const KeyboardCloseAndSnapshot: Story = { args: { state: 'nested' }, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  await userEvent.click(canvas.getByRole('button', { name: 'Reset layout' }))
  await settle()
  const neighbour = canvasElement.querySelector('[data-pane-key="agent:worker-1 view=secondary"]')!
  const editor = neighbour.querySelector<HTMLTextAreaElement>('[data-testid="composer-input"]')!
  await userEvent.clear(editor)
  await userEvent.type(editor, 'Keep the secondary-view draft')
  const close = canvas.getByRole('button', { name: 'Close Changes' })
  close.focus()
  await userEvent.keyboard('{Enter}')
  await settle()
  await expect(canvas.queryByTestId('diff-panel')).toBeNull()
  await expect(canvasElement.querySelector('[data-pane-key="agent:worker-1 view=secondary"]')).toBe(neighbour)
  await expect(editor.value).toBe('Keep the secondary-view draft')
  await expect(document.activeElement?.closest('[data-pane-key]')?.getAttribute('data-pane-key')).toBe('agent:worker-1 view=secondary')
  const root = canvas.getByTestId('workbench-story')
  const current = JSON.parse(root.dataset.layout!) as WorkbenchLayout
  await expect(current.kind === 'split' && current.ratio).toBe(0.62)
  await expect(canvasElement.querySelector('[data-resizer="split-0.1"]')?.getAttribute('aria-valuenow')).toBe('70')
  await expect(JSON.parse(localStorage.getItem(`workbench.${root.dataset.workspace}.snapshot`)!)).toEqual(current)
  const separator = canvasElement.querySelector<HTMLElement>('[data-resizer="split-0.1"]')!
  separator.focus()
  await userEvent.keyboard('{ArrowRight}')
  await settle()
  await expect(separator.getAttribute('aria-valuenow')).toBe('78')
} }
export const KeyboardTabsAndTerminal: Story = { args: { state: 'tabs' }, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  await userEvent.click(canvas.getByRole('button', { name: 'Reset layout' }))
  const first = canvas.getByRole('tab', { name: 'worker-1' })
  first.focus()
  await userEvent.keyboard('{ArrowRight}')
  await settle()
  const second = canvas.getByRole('tab', { name: 'worker-2' })
  await expect(second).toHaveAttribute('aria-selected', 'true')
  await userEvent.keyboard('{Delete}')
  await settle()
  await expect(canvas.queryByRole('tab', { name: 'worker-2' })).toBeNull()
  const before = canvas.getByTestId('workbench-story').dataset.layout
  const input = canvas.getByTestId('composer-input')
  await userEvent.clear(input)
  await userEvent.type(input, 'Keep this draft while opening terminal')
  await userEvent.click(canvas.getByRole('button', { name: 'Open terminal' }))
  await expect(canvas.getByTestId('terminal-drawer')).toBeVisible()
  await expect(canvas.getByTestId('workbench-story').dataset.layout).toBe(before)
  await expect(input).toHaveValue('Keep this draft while opening terminal')
  await userEvent.click(canvas.getByRole('button', { name: 'Hide terminal' }))
  await expect(canvas.queryByTestId('terminal-drawer')).toBeNull()
} }
export const KeyboardSplitPreservesHost: Story = { args: { state: 'one' }, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  await userEvent.click(canvas.getByRole('button', { name: 'Reset layout' }))
  const host = canvasElement.querySelector('[data-pane-key="agent:worker-1"]')!
  const editor = canvas.getByTestId('composer-input')
  await userEvent.clear(editor)
  await userEvent.type(editor, 'Preserve across split')
  await userEvent.click(canvas.getByRole('button', { name: 'worker-2 actions' }))
  await userEvent.click(within(document.body).getByRole('menuitem', { name: 'Open in split right' }))
  await settle()
  await expect(canvasElement.querySelector('[data-pane-key="agent:worker-1"]')).toBe(host)
  await expect(host.querySelector('[data-testid="composer-input"]')).toBe(editor)
  await expect(editor).toHaveValue('Preserve across split')
  await expect(canvas.getByRole('separator', { name: 'Resize split panes' })).toHaveAttribute('aria-valuenow', '50')
} }
export const HostToolReveal: Story = { args: { state: 'two' }, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  await userEvent.click(canvas.getByRole('button', { name: 'Reset layout' }))
  const file = canvas.getByRole('button', { name: 'sample/rows.ts' })
  await userEvent.click(file)
  await expect(file).toHaveAttribute('aria-expanded', 'false')
  await userEvent.click(canvas.getByRole('button', { name: /^Read sample\/rows\.ts\b/ }))
  await userEvent.click(canvas.getByRole('button', { name: 'Open Read sample/rows.ts tool detail' }))
  await settle()
  await expect(file).toHaveAttribute('aria-expanded', 'true')
  await userEvent.click(file)
  await userEvent.click(canvas.getByRole('button', { name: 'Open Read sample/rows.ts tool detail' }))
  await settle()
  await expect(file).toHaveAttribute('aria-expanded', 'true')
} }
export const AllStates: Story = { render: args => <main aria-label="Workbench states" {...stylex.props(styles.matrix)}>{(['one', 'two', 'three', 'dragging'] as const).map(state => <section key={state} {...stylex.props(styles.specimen)}><h2>{state} · W3/H1/P3/D2</h2><WorkbenchStory {...args} state={state} persist={false} /></section>)}</main>, play: async ({ canvasElement }) => {
  await settle()
  // Syntax tokens belong to kit A; these assertions target newly reachable UI counts, markers and washes.
  const chrome = canvasElement.querySelectorAll('[data-testid="pane-header"] span, [data-testid="diff-panel"] span, [data-testid="diff-code"] > div')
  for (const element of chrome) {
    if (element.closest('code')) continue
    const style = getComputedStyle(element)
    for (const value of [style.color, style.backgroundColor, style.borderLeftColor]) {
      const channels = value.match(/[\d.]+/g)?.map(Number)
      if (channels !== undefined && channels.length >= 3) await expect(channels[1]! > channels[0]! + 6 && channels[1]! > channels[2]! + 6).toBe(false)
    }
  }
} }
export const AllStatesLight: Story = { ...AllStates, args: { scheme: 'light' } }
const styles = stylex.create({
  canvas: { display: 'flex', flexDirection: 'column', height: '100vh', minWidth: 0, overflow: 'hidden', backgroundColor: surface.canvas, color: text.fg, fontFamily: t.fontSans, fontSize: t.metaSize },
  toolbar: { display: 'flex', alignItems: 'center', flexWrap: 'wrap', gap: s.lg, padding: s.sm, flexShrink: 0, borderBottomWidth: g.hairline, borderBottomStyle: 'solid', borderBottomColor: border.border }, workspace: { display: 'flex', flex: '1 1 0', minHeight: 0, minWidth: 0 },
  button: { minHeight: g.controlMd, paddingInline: s.sm, backgroundColor: surface.controlFill, color: text.fg, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, cursor: 'pointer', ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary } },
  override: { display: 'flex', flexDirection: 'column', gap: s.md, padding: s.lg }, matrix: { display: 'flex', flexDirection: 'column', gap: s.xl }, specimen: { minWidth: 0 },
})
