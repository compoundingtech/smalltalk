import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { expect, userEvent, waitFor, within } from 'storybook/test'
import { LiveState } from './assistant-ui/live-state/LiveState'
import { diagnosticFixture, diagnosticNow, observationAxes, type DiagnosticScenario } from './assistant-ui/live-state/fixtures'
import { ThreadHeader } from './assistant-ui/composition/Shell'
import { ThemePortal } from './assistant-ui/taste/ThemePortal'
import { baselineTheme } from './assistant-ui/neutral-theme'
import { lightTheme, type Scheme } from './assistant-ui/composition-theme'
import { borderVars as border, geometryVars as g, spaceVars as s, surfaceVars as surface, textVars as ink, typeVars as t } from './assistant-ui/composition-tokens.stylex'

interface Args { scenario: DiagnosticScenario; scheme: Scheme; control: boolean }
/** An explicit host clock: subscription lifetime owns the interval, deterministic producer time. */
function createClock() {
  let now = diagnosticNow, timer: number | undefined
  const listeners = new Set<() => void>()
  return {
    getSnapshot: () => now,
    subscribe: (listener: () => void) => {
      listeners.add(listener)
      timer ??= window.setInterval(() => { now += 1000; for (const notify of listeners) notify() }, 1000)
      return () => { listeners.delete(listener); if (listeners.size === 0) { window.clearInterval(timer); timer = undefined } }
    },
  }
}
function DiagnosticSurface({ scenario, scheme, control }: Args) {
  const [clock] = React.useState(createClock)
  const clockNow = React.useSyncExternalStore(clock.subscribe, clock.getSnapshot, clock.getSnapshot)
  const now = control && ['long-tool', 'quota-countdown', 'retry-countdown'].includes(scenario) ? diagnosticNow : clockNow
  const snapshot = diagnosticFixture(scenario, control)
  const keyboardControl = React.useCallback((element: HTMLElement | null) => {
    if (element !== null && scenario === 'keyboard') for (const button of element.querySelectorAll<HTMLButtonElement>('button[aria-label^="Details:"]')) button.tabIndex = control ? -1 : 0
  }, [scenario, control])
  return <section ref={keyboardControl} data-testid="diagnostic-surface" data-scenario={scenario} data-scheme={scheme} {...stylex.props(styles.surface, ...baselineTheme, scheme === 'light' && lightTheme)}><ThemePortal>
    <h2 {...stylex.props(styles.heading, control && scenario === 'reduced-motion' && styles.motionControl)}>{scenario.replaceAll('-', ' ')} · {scheme}</h2>
    <div role="list" aria-label="Thread list" {...stylex.props(styles.list)}><div role="listitem" {...stylex.props(styles.threadRow)}><strong {...stylex.props(styles.threadTitle)}>Verify the interface contract</strong><LiveState label="Iris row" snapshot={snapshot} now={now} /></div></div>
    <div {...stylex.props(styles.thread)}><ThreadHeader folder="workspace" title="Iris header" landmarkContext={`Iris ${scenario}, ${scheme}`} diagnostics={{ snapshot, now }} panelOpen={false} drawerOpen={false} onTogglePanel={() => {}} onToggleDrawer={() => {}} /><p {...stylex.props(styles.note)}>One admitted execution snapshot, independent diagnostic axes. Details retain provenance, source support and task blockers.</p></div>
  </ThemePortal></section>
}
function DiagnosticStory(args: Args) { return <main {...stylex.props(styles.root)}><DiagnosticSurface {...args} /></main> }
const meta = { title: 'Fractal UI/Live state', component: DiagnosticStory, parameters: { layout: 'fullscreen' }, args: { scenario: 'all-known', scheme: 'dark', control: false }, argTypes: { scheme: { control: 'radio', options: ['dark', 'light'] }, control: { control: 'boolean', description: 'Failing control: removes the observation asserted by the scenario play.' }, scenario: { control: false } } } satisfies Meta<typeof DiagnosticStory>
export default meta
type Story = StoryObj<typeof meta>
const expectedPrimary: Partial<Record<DiagnosticScenario, string>> = { 'all-known': 'needs_you', 'needs-you': 'needs_you', keyboard: 'needs_you', 'reduced-motion': 'needs_you', offline: 'host_reachability', crash: 'harness_exit', 'quota-countdown': 'quota_retry', 'retry-countdown': 'quota_retry', 'long-tool': 'activity', 'missing-tool-start': 'activity', concurrent: 'activity', 'stale-activity': 'progress', 'expired-activity': 'progress', 'progress-only': 'progress', 'runtime-only': 'runtime', 'heartbeat-only': 'heartbeat' }
const expectedAxisCopy: Record<string, string> = { runtime: 'Runtime running', activity: 'Tool: shell', needs_you: 'Needs you: permission', quota_retry: 'Retry attempt 3', progress: '1/4 tasks complete', heartbeat: 'Heartbeat', host_reachability: 'Host online', harness_exit: 'No harness exit reported' }
async function assertNoPlaceholder(root: HTMLElement) {
  await expect(root.textContent).not.toMatch(/unknown/i)
  for (const node of [root, ...root.querySelectorAll('[aria-label], [aria-description], [title]')]) {
    await expect([node.getAttribute('aria-label'), node.getAttribute('aria-description'), node.getAttribute('title')].filter(Boolean).join(' ')).not.toMatch(/unknown/i)
  }
}
const axisPlay: NonNullable<Story['play']> = async ({ canvasElement, args }) => {
  const scenario = args.scenario
  const surface = within(canvasElement).getByTestId('diagnostic-surface')
  const row = surface.querySelector<HTMLElement>('[data-variant="row"]')!
  const header = surface.querySelector<HTMLElement>('[data-variant="header"]')!
  await assertNoPlaceholder(surface)
  if (expectedPrimary[scenario] !== undefined) await expect(row).toHaveAttribute('data-primary-axis', expectedPrimary[scenario])
  if (scenario === 'all-known' || scenario === 'keyboard' || scenario === 'reduced-motion') {
    await expect(header.querySelectorAll('[data-diagnostic-axis]').length).toBe(8)
    for (const axis of observationAxes) await expect(header.querySelector(`[data-diagnostic-axis="${axis}"]`)?.textContent).toContain(expectedAxisCopy[axis])
  }
  if (scenario === 'crash') await expect(row).toHaveTextContent('Harness crashed')
  if (scenario === 'offline') await expect(row).toHaveTextContent('Host offline')
  if (scenario === 'needs-you') await expect(row).toHaveTextContent('Needs you: permission')
  if (scenario === 'stale-heartbeat') {
    const heartbeat = header.querySelector('[data-diagnostic-axis="heartbeat"]')!
    await expect(heartbeat).toHaveAttribute('data-stale', 'true')
    await expect(heartbeat).toHaveTextContent(/Stale heartbeat · 3m/)
  }
  if (scenario === 'stale-activity' || scenario === 'expired-activity') {
    const activity = header.querySelector('[data-diagnostic-axis="activity"]')!
    await expect(activity).toHaveAttribute('data-stale', 'true')
    await expect(activity).toHaveTextContent(/stale 3m/)
    await expect(within(activity as HTMLElement).queryByRole('timer')).toBeNull()
  }
  if (scenario === 'unsupported') {
    await expect(row).toHaveAttribute('data-primary-axis', 'none')
    await expect(header.querySelectorAll('[data-diagnostic-axis]').length).toBe(0)
  }
  if (scenario.startsWith('missing-') && scenario !== 'missing-tool-start') await expect(header.querySelector(`[data-diagnostic-axis="${scenario.slice(8)}"]`)).toBeNull()
  if (scenario === 'missing-tool-start') await expect(within(row).queryByRole('timer')).toBeNull()
  if (scenario === 'long-tool' || scenario === 'quota-countdown' || scenario === 'retry-countdown') {
    const timer = within(row).getByRole('timer'), before = timer.textContent
    const bounds = timer.getBoundingClientRect(), rowBounds = row.getBoundingClientRect()
    await expect(timer).toHaveTextContent(scenario === 'long-tool' ? /(?:9h 59m|10h 00m)/ : /1m 0[0-5]s/)
    await waitFor(async () => { await expect(timer.textContent).not.toBe(before) }, { timeout: 2500 })
    await expect(timer.getBoundingClientRect().width).toBe(bounds.width)
    await expect(timer.getBoundingClientRect().x).toBe(bounds.x)
    await expect(row.getBoundingClientRect().height).toBe(rowBounds.height)
    await expect(row.getBoundingClientRect().width).toBe(rowBounds.width)
  }
  if (scenario === 'reduced-motion') {
    await expect(window.matchMedia('(prefers-reduced-motion: reduce)').matches).toBe(true)
    for (const element of surface.querySelectorAll('*')) await expect(getComputedStyle(element).animationName).toBe('none')
  }
  const trigger = within(row).getByRole('button', { name: 'Details: full diagnostics for Iris row' })
  // Tab reaches the real React Aria trigger; Enter opens and Escape returns focus.
  const previous = surface.ownerDocument.createElement('button')
  previous.textContent = 'Focus anchor'; row.before(previous); previous.focus()
  await userEvent.tab()
  await expect(trigger).toHaveFocus()
  previous.remove()
  await userEvent.keyboard('{Enter}')
  const dialog = await within(surface.ownerDocument.body).findByRole('dialog', { name: 'Full diagnostics for Iris row' })
  await expect(dialog).toBeVisible()
  await assertNoPlaceholder(dialog)
  await expect(dialog.querySelectorAll('[data-detail-axis]').length).toBe(8)
  if (scenario === 'all-known') for (const axis of observationAxes) await expect(dialog.querySelector(`[data-detail-axis="${axis}"]`)?.textContent).toContain(expectedAxisCopy[axis])
  if (scenario.startsWith('missing-') && scenario !== 'missing-tool-start') await expect(dialog.querySelector(`[data-detail-axis="${scenario.slice(8)}"]`)?.textContent).toContain("isn't reported: the observation source was lost")
  if (scenario === 'unsupported') for (const axis of observationAxes) await expect(dialog.querySelector(`[data-detail-axis="${axis}"]`)?.textContent).toContain("isn't reported by this adapter")
  if (scenario === 'missing-tool-start') await expect(dialog.querySelector('[data-detail-axis="activity"]')?.textContent).toContain("Tool start isn't reported")
  if (scenario === 'concurrent') {
    await expect(dialog.querySelector('[data-detail-axis="activity"]')?.textContent).toContain('Tool: shell')
    await expect(dialog.querySelector('[data-detail-axis="activity"]')?.textContent).toContain('Thinking')
    await expect(dialog.querySelector('[data-detail-axis="activity"]')?.textContent).toContain('request-lumen')
  }
  if (scenario === 'milestones') await expect(dialog.querySelector('[data-detail-axis="progress"]')?.textContent).toContain('2/5 review gates')
  if (scenario === 'percent') await expect(dialog.querySelector('[data-detail-axis="progress"]')?.textContent).toContain('35% · producer-reported verification')
  if (scenario === 'truncated-tasks') await expect(dialog.querySelector('[data-detail-axis="progress"]')?.textContent).toContain('truncated')
  await expect(dialog.querySelector('[aria-label="Execution identity"]')?.textContent).toContain('execution-iris')
  await userEvent.keyboard('{Escape}')
  await waitFor(async () => { await expect(within(surface.ownerDocument.body).queryByRole('dialog')).toBeNull() })
  await waitFor(async () => { await expect(trigger).toHaveFocus() })
  await assertNoPlaceholder(surface)
}
export const AllKnown: Story = { play: axisPlay }
export const RuntimeNotReported: Story = { args: { scenario: 'missing-runtime' }, play: axisPlay }
export const ActivityNotReported: Story = { args: { scenario: 'missing-activity' }, play: axisPlay }
export const NeedsYouNotReported: Story = { args: { scenario: 'missing-needs_you' }, play: axisPlay }
export const QuotaRetryNotReported: Story = { args: { scenario: 'missing-quota_retry' }, play: axisPlay }
export const ProgressNotReported: Story = { args: { scenario: 'missing-progress' }, play: axisPlay }
export const HeartbeatNotReported: Story = { args: { scenario: 'missing-heartbeat' }, play: axisPlay }
export const HostNotReported: Story = { args: { scenario: 'missing-host_reachability' }, play: axisPlay }
export const CrashNotReported: Story = { args: { scenario: 'missing-harness_exit' }, play: axisPlay }
export const Unsupported: Story = { args: { scenario: 'unsupported' }, play: axisPlay }
export const StaleHeartbeat: Story = { args: { scenario: 'stale-heartbeat' }, play: axisPlay }
export const StaleActivity: Story = { args: { scenario: 'stale-activity' }, play: axisPlay }
export const ExpiredActivity: Story = { args: { scenario: 'expired-activity' }, play: axisPlay }
export const HostOffline: Story = { args: { scenario: 'offline' }, play: axisPlay }
export const Crash: Story = { args: { scenario: 'crash' }, play: axisPlay }
export const NeedsYou: Story = { args: { scenario: 'needs-you' }, play: axisPlay }
export const QuotaCountdown: Story = { args: { scenario: 'quota-countdown' }, play: axisPlay }
export const RetryCountdown: Story = { args: { scenario: 'retry-countdown' }, play: axisPlay }
export const LongToolElapsed: Story = { args: { scenario: 'long-tool' }, play: axisPlay }
export const ToolStartNotReported: Story = { args: { scenario: 'missing-tool-start' }, play: axisPlay }
export const ProgressOnly: Story = { args: { scenario: 'progress-only' }, play: axisPlay }
export const RuntimeOnly: Story = { args: { scenario: 'runtime-only' }, play: axisPlay }
export const HeartbeatOnly: Story = { args: { scenario: 'heartbeat-only' }, play: axisPlay }
export const Milestones: Story = { args: { scenario: 'milestones' }, play: axisPlay }
export const ReportedPercent: Story = { args: { scenario: 'percent' }, play: axisPlay }
export const ConcurrentActivity: Story = { args: { scenario: 'concurrent' }, play: axisPlay }
export const TruncatedTasks: Story = { args: { scenario: 'truncated-tasks' }, play: axisPlay }
export const KeyboardDetails: Story = { args: { scenario: 'keyboard' }, play: axisPlay }
export const ReducedMotion: Story = { args: { scenario: 'reduced-motion' }, play: axisPlay }
function AllStatesStory({ control }: Args) {
  const scenarios: readonly DiagnosticScenario[] = ['all-known', 'offline', 'crash', 'quota-countdown', 'long-tool', 'stale-heartbeat', 'stale-activity', 'unsupported', 'missing-tool-start', 'concurrent', 'milestones', 'percent']
  return <main {...stylex.props(styles.root)}><h1 {...stylex.props(styles.heading)}>Live state · all states</h1><div {...stylex.props(styles.allStates)}>{(['dark', 'light'] as const).map(scheme => <section key={scheme} aria-label={`${scheme} diagnostic states`}>{scenarios.map(scenario => <DiagnosticSurface key={scenario} scenario={scenario} scheme={scheme} control={control} />)}</section>)}</div></main>
}
export const AllStates: Story = { render: args => <AllStatesStory {...args} />, play: async ({ canvasElement }) => {
  await expect(canvasElement.querySelectorAll('[data-testid="diagnostic-surface"]').length).toBe(24)
  for (const scheme of ['dark', 'light']) {
    const surface = within(canvasElement).getByRole('region', { name: `${scheme} diagnostic states` })
    await expect(surface.querySelector('[data-scenario="all-known"] [data-variant="row"]')).toHaveAttribute('data-primary-axis', 'needs_you')
    await expect(surface.querySelector('[data-scenario="crash"] [data-variant="row"]')).toHaveAttribute('data-primary-axis', 'harness_exit')
    await expect(surface.querySelector('[data-scenario="offline"] [data-variant="row"]')).toHaveAttribute('data-primary-axis', 'host_reachability')
    const activityText = surface.querySelector<HTMLElement>('[data-scenario="long-tool"] [data-variant="header"] [data-diagnostic-axis="activity"] > span')!
    await expect(activityText.clientWidth).toBeGreaterThanOrEqual(activityText.scrollWidth)
    await assertNoPlaceholder(surface)
  }
} }
const styles = stylex.create({
  motionControl: { animationName: stylex.keyframes({ to: { transform: 'rotate(360deg)' } }), animationDuration: '1s', animationIterationCount: 'infinite' },
  root: { minHeight: '100vh', padding: s.xl, boxSizing: 'border-box', backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans },
  surface: { padding: s.lg, marginBottom: s.lg, backgroundColor: surface.canvas, color: ink.fg, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong },
  heading: { marginBlock: s.md, fontSize: t.uiSize, fontWeight: t.weightMedium }, list: { width: '360px', maxWidth: '100%', marginBlock: s.lg, backgroundColor: surface.sidebar },
  threadRow: { padding: s.md }, threadTitle: { display: 'block', fontSize: t.metaSize, marginBottom: s.xs }, thread: { borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.border },
  note: { margin: s.lg, color: ink.fgMuted, fontSize: t.metaSize }, allStates: { display: 'grid', gridTemplateColumns: 'repeat(2, minmax(0, 1fr))', gap: s.lg },
})
