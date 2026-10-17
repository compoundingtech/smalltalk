import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { expect, userEvent, waitFor, within } from 'storybook/test'
import { Button } from 'react-aria-components'
import { EmbraceScrollViewport } from './assistant-ui/EmbraceScrollViewport'
import { SidebarAgentRow } from './assistant-ui/sidebar/SidebarAgentRow'
import type { AgentStatus, SidebarAgentRow as Row } from './assistant-ui/sidebar/model'
import { ThreadHeader } from './assistant-ui/composition/Shell'
import { ConnectionStatus, SystemNotice, type ConnectionState } from './assistant-ui/composition/SystemState'
import { baselineTheme } from './assistant-ui/neutral-theme'
import { darkTheme, liveAccentTheme } from './assistant-ui/embrace-theme'
import { lightTheme, type Scheme } from './assistant-ui/composition-theme'
import { accentVars as accent, borderVars as border, radiusVars as r, surfaceVars as surface, textVars as ink, spaceVars as s, typeVars as t, geometryVars as g } from './assistant-ui/composition-tokens.stylex'

const now = Date.parse('2026-01-15T12:00:00Z')
const minutes = (count: number) => now - count * 60_000
/** The roster as last observed before the connection dropped: the app marks every row stale. */
const agent = (name: string, host: string, status: AgentStatus, since: number): Row => ({
  ref: `agent/${name}`, id: `agent/${name}`, title: name, host, status, statusLabel: status, freshness: 'stale', harness: 'omp',
  statusSince: minutes(since), lastActivityAt: minutes(since), lastTurn: { _tag: 'Known', kind: 'turn-completed', at: minutes(since) },
  childrenKnown: false, children: [], usage: { _tag: 'Unknown' }, duration: { _tag: 'Unknown' },
})
const rows: readonly Row[] = [
  agent('fractal-web', 'workstation-a', 'working', 12),
  agent('st-gateway', 'build-host-a', 'idle', 95),
  agent('ui-design', 'workstation-a', 'waiting', 4),
  agent('hypermerge', 'build-host-a', 'working', 31),
  agent('nix-infra', 'build-host-b', 'idle', 240),
  agent('pr-review', 'build-host-b', 'waiting', 18),
]
const countdown = 'Next try in 2s · attempt 1'

/**
 * The workspace as the app composes its system states: one amber band under the thread header owns the gateway
 * connection; the roster (last observed, stale), Missions (empty), Changes (loading) and the terminal (ended, with
 * Retry) keep quiet one-line rows and never repeat the connection. Reconnect and Retry are live for the plays.
 */
function SystemStatesStory({ scheme, connection: initial, failure, ticking = false }: { scheme: Scheme; connection: ConnectionState; failure?: string; ticking?: boolean }) {
  const [connection, setConnection] = React.useState<ConnectionState>(initial)
  const [terminal, setTerminal] = React.useState<'ended' | 'loading'>('ended')
  const countdownText = useCountdown(ticking && connection === 'reconnecting')
  const reconnect = () => setConnection('reconnecting')
  const text = connection === 'failed' ? failure : countdownText
  return <main aria-label="Workspace" data-scheme={scheme} {...stylex.props(styles.root, ...baselineTheme, scheme === 'dark' && darkTheme, liveAccentTheme, scheme === 'light' && lightTheme)}>
    <aside aria-label="Agents" {...stylex.props(styles.sidebar)}>
      <header {...stylex.props(styles.brand)}><strong>Fractal</strong></header>
      <nav aria-label="Agent roster" {...stylex.props(styles.roster)}>
        {rows.map(row => <SidebarAgentRow key={row.ref} item={row} now={now} active={row.ref === 'agent/fractal-web'} onOpen={() => {}} />)}
      </nav>
      <SystemNotice scope="rail" kind="stale" connection title="Last observed roster" detail="Live updates pending" />
    </aside>
    <section aria-label="Agent workspace" {...stylex.props(styles.workspace)}>
      <ThreadHeader folder="workstation-a" title="fractal-web" status="working" statusLabel="working" statusSince={minutes(12)} freshness="stale" now={now} terminalAvailable panelOpen drawerOpen onTogglePanel={() => {}} onToggleDrawer={() => {}} />
      <ConnectionStatus state={connection} {...(text === undefined ? {} : { text })} {...(connection === 'offline' || connection === 'reconnecting' ? { onReconnect: reconnect } : {})} />
      <div {...stylex.props(styles.body)}>
        <section aria-label="Missions" {...stylex.props(styles.missions)}>
          <header {...stylex.props(styles.paneHeader)}>Missions</header>
          <div {...stylex.props(styles.paneBody)}>
            <SystemNotice kind="empty" title="No missions" />
          </div>
        </section>
        <aside aria-label="Changes" {...stylex.props(styles.changes)}>
          <SystemNotice kind="loading" title="Loading captured changes" />
        </aside>
      </div>
      <section aria-label="Terminal drawer" {...stylex.props(styles.drawer)}>
        {terminal === 'ended'
          ? <SystemNotice kind="error" title="Terminal session ended" detail="The shell exited with code 137" onRetry={() => setTerminal('loading')} />
          : <SystemNotice kind="loading" title="Loading terminal" />}
      </section>
    </section>
  </main>
}

/** The host's countdown: static for shots, or ticking every second (3s, 2s, 1s, then the next attempt) for the announcement play. */
function useCountdown(ticking: boolean) {
  const [tick, setTick] = React.useState(0)
  React.useEffect(() => {
    if (!ticking) return
    const timer = setInterval(() => setTick(value => value + 1), 1000)
    return () => clearInterval(timer)
  }, [ticking])
  return ticking ? `Next try in ${3 - (tick % 3)}s · attempt ${1 + Math.floor(tick / 3)}` : countdown
}

const transcript = Array.from({ length: 48 }, (_, index) => ({
  id: `turn-${index}`,
  who: index % 2 === 0 ? 'You' : 'fractal-web',
  text: index % 2 === 0 ? `Check step ${index / 2 + 1} of the reconnect path and tell me what you observe.` : `Step ${(index + 1) / 2}: the gateway answered, the roster refreshed and the pane kept its rows; nothing on screen was replaced while the socket was away.`,
}))
/**
 * A populated conversation under the band, on the kit's transcript scroller. The band enters and leaves above the
 * history: a reader following the end stays at the end; a reader up in history keeps their place.
 */
function ConnectionGeometryStory({ scheme }: { scheme: Scheme }) {
  const [connection, setConnection] = React.useState<ConnectionState>('live')
  return <main aria-label="Workspace" data-scheme={scheme} {...stylex.props(styles.root, ...baselineTheme, scheme === 'dark' && darkTheme, liveAccentTheme, scheme === 'light' && lightTheme)}>
    <section aria-label="Agent workspace" {...stylex.props(styles.workspace)}>
      <ThreadHeader folder="workstation-a" title="fractal-web" status="working" statusLabel="working" statusSince={minutes(12)} freshness={connection === 'live' ? 'live' : 'stale'} now={now} panelOpen={false} drawerOpen={false} onTogglePanel={() => {}} onToggleDrawer={() => {}} />
      <ConnectionStatus state={connection} {...(connection === 'offline' ? { onReconnect: () => setConnection('live') } : {})} />
      <EmbraceScrollViewport items={transcript} data-testid="transcript-scroll" aria-label="Conversation history" tabIndex={0} {...stylex.props(styles.lane)} contentProps={stylex.props(styles.turns)}>
        {transcript.map(turn => <article key={turn.id} data-item-id={turn.id} {...stylex.props(styles.turn)}><strong {...stylex.props(styles.turnWho)}>{turn.who}</strong><p {...stylex.props(styles.turnText)}>{turn.text}</p></article>)}
      </EmbraceScrollViewport>
      <div role="toolbar" aria-label="Story connection" {...stylex.props(styles.harness)}>
        <Button onPress={() => setConnection('offline')} {...stylex.props(styles.harnessButton)}>Drop connection</Button>
        <Button onPress={() => setConnection('live')} {...stylex.props(styles.harnessButton)}>Restore connection</Button>
      </div>
    </section>
  </main>
}

const meta = {
  title: 'Fractal UI/System states',
  component: SystemStatesStory,
  parameters: { layout: 'fullscreen' },
  args: { scheme: 'dark', connection: 'offline' },
  argTypes: { scheme: { options: ['dark', 'light'], control: 'radio' }, connection: { options: ['live', 'offline', 'reconnecting', 'stale', 'failed'], control: 'radio' }, failure: { control: 'text' }, ticking: { control: 'boolean' } },
} satisfies Meta<typeof SystemStatesStory>
export default meta
type Story = StoryObj<typeof meta>

const band = (canvasElement: HTMLElement) => canvasElement.querySelectorAll('[data-connection-state]')
/** One band under the thread header, none in the sidebar; every pane keeps its own quiet row. */
const oneBand = (state: ConnectionState) => async ({ canvasElement }: { canvasElement: HTMLElement }) => {
  await expect(band(canvasElement)).toHaveLength(1)
  const workspace = within(canvasElement).getByRole('region', { name: 'Agent workspace' })
  await expect(workspace.querySelector(`[data-connection-state="${state}"]`)).not.toBeNull()
  await expect(canvasElement.querySelector('aside[aria-label="Agents"] [data-connection-state]')).toBeNull()
  await expect(canvasElement.querySelectorAll('[data-system-state]')).toHaveLength(4)
  await expect(canvasElement.querySelector('[data-system-state="stale"]')).toHaveTextContent('Last observed roster')
  await expect(canvasElement.querySelector('[data-system-state="error"]')).toHaveTextContent('Terminal session ended. The shell exited with code 137')
}

/** Offline: the band names it once and offers Reconnect; the roster row says only what it still shows. */
export const ConnectionBanner: Story = { play: async context => {
  await oneBand('offline')(context)
  await expect(band(context.canvasElement)[0]).toHaveTextContent('You are offline')
} }
export const ConnectionBannerLight: Story = { ...ConnectionBanner, args: { scheme: 'light' } }
/** Reconnecting: the band carries the host's countdown. */
export const Reconnecting: Story = { args: { connection: 'reconnecting' }, play: async context => {
  await oneBand('reconnecting')(context)
  await expect(band(context.canvasElement)[0]).toHaveTextContent(countdown)
} }
/** Reconnecting with a live countdown: the detail ticks, the live region keeps one headline, so nothing is re-announced. */
export const ReconnectingAnnouncesOnce: Story = { args: { connection: 'reconnecting', ticking: true }, play: async ({ canvasElement }) => {
  const reconnecting = band(canvasElement)[0]!
  const status = reconnecting.querySelector<HTMLElement>('[role="status"]')!
  const detail = reconnecting.querySelector<HTMLElement>('[data-connection-detail]')!
  await expect(status).toHaveTextContent(/^Reconnecting$/)
  await expect(status.contains(detail)).toBe(false)
  const announced: string[] = []
  const watcher = new MutationObserver(() => announced.push(status.textContent ?? ''))
  watcher.observe(status, { childList: true, characterData: true, subtree: true })
  const first = detail.textContent
  // Two ticks of the countdown: the detail changes, the live region does not.
  await waitFor(() => expect(detail.textContent).not.toBe(first), { timeout: 2500 })
  const second = detail.textContent
  await waitFor(() => expect(detail.textContent).not.toBe(second), { timeout: 2500 })
  watcher.disconnect()
  await expect(announced).toEqual([])
  await expect(status).toHaveTextContent(/^Reconnecting$/)
} }
/** Failed: the generic headline, the decoded safe reason as the detail, and no Reconnect because no retry is pending. */
export const ConnectionFailed: Story = { args: { connection: 'failed', failure: 'No access to connection · ask an administrator' }, play: async context => {
  await oneBand('failed')(context)
  const failed = band(context.canvasElement)[0]!
  await expect(failed.querySelector('[role="status"]')).toHaveTextContent(/^Connection lost$/)
  await expect(failed.querySelector('[data-connection-detail]')).toHaveTextContent('No access to connection · ask an administrator')
  await expect(within(context.canvasElement).queryByRole('button', { name: 'Reconnect' })).toBeNull()
} }
/** Failed because the gateway does not offer the connection here: the reason names that, not a generic retry. */
export const ConnectionUnavailable: Story = { args: { connection: 'failed', failure: 'connection is not available here' }, play: async context => {
  await oneBand('failed')(context)
  await expect(band(context.canvasElement)[0]!.querySelector('[data-connection-detail]')).toHaveTextContent('connection is not available here')
} }
/** Failed with no decoded reason: the band falls back to what is still on screen. */
export const ConnectionFailedNoReason: Story = { args: { connection: 'failed' }, play: async context => {
  await oneBand('failed')(context)
  await expect(band(context.canvasElement)[0]!.querySelector('[data-connection-detail]')).toHaveTextContent('Showing what was last observed')
} }
/** Live: no band at all; the panes' own rows stay. */
export const Live: Story = { args: { connection: 'live' }, play: async ({ canvasElement }) => {
  await expect(band(canvasElement)).toHaveLength(0)
  await expect(canvasElement.querySelectorAll('[data-system-state]')).toHaveLength(4)
} }
/** Reconnect moves the band to reconnecting; Retry on the terminal row loads the terminal again. */
export const ConnectionBannerRecovers: Story = { play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  await userEvent.click(canvas.getByRole('button', { name: 'Reconnect' }))
  await expect(canvasElement.querySelector('[data-connection-state="reconnecting"]')).toHaveTextContent(`Reconnecting·${countdown}`)
  await userEvent.click(canvas.getByRole('button', { name: 'Retry: Terminal session ended' }))
  const drawer = canvas.getByRole('region', { name: 'Terminal drawer' })
  await expect(drawer.querySelector('[data-system-state="loading"]')).toHaveTextContent('Loading terminal')
} }

const geometryFrame = () => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve())))
const distanceFromEnd = (viewport: HTMLElement) => viewport.scrollHeight - viewport.clientHeight - viewport.scrollTop
/** Drops the connection, checks the band is in, restores it, checks the band is gone. */
const dropAndRestore = async (canvasElement: HTMLElement, between: () => Promise<void>) => {
  const canvas = within(canvasElement)
  await userEvent.click(canvas.getByRole('button', { name: 'Drop connection' }))
  await waitFor(() => expect(band(canvasElement)).toHaveLength(1))
  await geometryFrame()
  await between()
  await userEvent.click(canvas.getByRole('button', { name: 'Restore connection' }))
  await waitFor(() => expect(band(canvasElement)).toHaveLength(0))
  await geometryFrame()
}
/** Following the end of a populated conversation: live → offline → live keeps the end in view (distance 0). */
export const BandKeepsFollow: Story = { render: ({ scheme }) => <ConnectionGeometryStory scheme={scheme} />, play: async ({ canvasElement }) => {
  const viewport = within(canvasElement).getByTestId('transcript-scroll')
  await waitFor(() => expect(viewport.scrollHeight).toBeGreaterThan(viewport.clientHeight * 2))
  await waitFor(() => expect(distanceFromEnd(viewport)).toBeLessThanOrEqual(1))
  await dropAndRestore(canvasElement, async () => {
    await waitFor(() => expect(distanceFromEnd(viewport)).toBeLessThanOrEqual(1))
  })
  await waitFor(() => expect(distanceFromEnd(viewport)).toBeLessThanOrEqual(1))
  await expect(within(canvasElement).queryByRole('button', { name: 'New messages ↓' })).toBeNull()
} }
/** Negative control: a reader up in history keeps their row in place while the band comes and goes; no jump to the end. */
export const BandKeepsReadingPosition: Story = { render: ({ scheme }) => <ConnectionGeometryStory scheme={scheme} />, play: async ({ canvasElement }) => {
  const viewport = within(canvasElement).getByTestId('transcript-scroll')
  await waitFor(() => expect(distanceFromEnd(viewport)).toBeLessThanOrEqual(1))
  viewport.focus()
  viewport.dispatchEvent(new WheelEvent('wheel', { deltaY: -400 }))
  viewport.scrollTop = Math.round(viewport.scrollHeight / 3)
  viewport.dispatchEvent(new Event('scroll'))
  await geometryFrame()
  const anchor = Array.from(viewport.querySelectorAll<HTMLElement>('[data-item-id]')).find(row => row.getBoundingClientRect().top >= viewport.getBoundingClientRect().top)!
  const offset = () => anchor.getBoundingClientRect().top - viewport.getBoundingClientRect().top
  const before = offset()
  await dropAndRestore(canvasElement, async () => {
    await expect(Math.abs(offset() - before)).toBeLessThanOrEqual(1)
    await expect(distanceFromEnd(viewport)).toBeGreaterThan(100)
  })
  await expect(Math.abs(offset() - before)).toBeLessThanOrEqual(1)
  await expect(distanceFromEnd(viewport)).toBeGreaterThan(100)
} }

// Narrow: the sidebar is closed and the Changes panel steps aside, so the workspace fills the phone.
const styles = stylex.create({
  root: { height: '100vh', width: '100%', boxSizing: 'border-box', display: 'flex', backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans, fontSize: t.uiSize, lineHeight: t.uiLeading },
  sidebar: { display: { default: 'flex', '@media (max-width: 600px)': 'none' }, flexDirection: 'column', flexShrink: 0, minHeight: 0, overflow: 'hidden', width: g.sidebarDefault, backgroundColor: surface.sidebar },
  brand: { height: g.band, paddingInline: s.lg, display: 'flex', alignItems: 'center', flexShrink: 0 },
  roster: { display: 'flex', flexDirection: 'column', overflowY: 'auto', padding: s.md, minHeight: 0, flexGrow: 1, gap: s.xs2 },
  workspace: { flex: '1 1 0', minWidth: 0, minHeight: 0, display: 'flex', flexDirection: 'column' },
  body: { flex: '1 1 0', minHeight: 0, display: 'flex' },
  missions: { flex: '1 1 0', minWidth: 0, display: 'flex', flexDirection: 'column' },
  paneHeader: { height: g.controlLg, display: 'flex', alignItems: 'center', paddingInline: s.lg, flexShrink: 0, fontSize: t.metaSize, fontWeight: t.weightMedium, borderBottomWidth: g.hairline, borderBottomStyle: 'solid', borderBottomColor: border.border },
  paneBody: { flex: '1 1 0', minHeight: 0, display: 'flex', flexDirection: 'column' },
  changes: { display: { default: 'flex', '@media (max-width: 900px)': 'none' }, flexDirection: 'column', width: '380px', flexShrink: 0, borderLeftWidth: g.hairline, borderLeftStyle: 'solid', borderLeftColor: border.border },
  drawer: { height: g.drawerDefault, flexShrink: 0, display: 'flex', flexDirection: 'column', borderTopWidth: g.hairline, borderTopStyle: 'solid', borderTopColor: border.border, backgroundColor: surface.terminal },
  lane: { flex: '1 1 0', minHeight: 0, overflowY: 'auto', overflowAnchor: 'none' },
  turns: { display: 'flex', flexDirection: 'column', gap: s.lg, maxWidth: '720px', marginInline: 'auto', padding: s.lg, boxSizing: 'border-box', width: '100%' },
  turn: { display: 'flex', flexDirection: 'column', gap: s.xs },
  turnWho: { fontSize: t.metaSize, fontWeight: t.weightMedium, color: ink.fgMuted },
  turnText: { margin: 0, fontSize: t.bodySize, lineHeight: t.bodyLeading },
  harness: { display: 'flex', gap: s.md, padding: s.md, flexShrink: 0, borderTopWidth: g.hairline, borderTopStyle: 'solid', borderTopColor: border.border },
  harnessButton: { minHeight: g.controlMd, paddingInline: s.md, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.control, backgroundColor: surface.controlFill, color: ink.fg, fontSize: t.metaSize, cursor: 'pointer', ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary } },
})
