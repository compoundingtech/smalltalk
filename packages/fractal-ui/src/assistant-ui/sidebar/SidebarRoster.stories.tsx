import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { Button } from 'react-aria-components'
import { expect, userEvent, within } from 'storybook/test'
import { SidebarAgentRow, type RowLayout } from './SidebarAgentRow'
import type { SidebarAgentRow as Row } from './model'
import { geometryVars as g, spaceVars as s, surfaceVars as surface, textVars as ink, typeVars as t } from '../composition-tokens.stylex'

const storyNow = Date.UTC(2026, 9, 8, 12, 0, 0)
const titles = ['Review queue', 'Align the sidebar metric track across every cohort row', 'Release notes', 'Port reported duration scopes into the hover card', 'Fix flaky gate', 'Measure the full roster render and keep fitting reads partitioned']
const work = [undefined, 'Reviews the selection model', 'Rebuilding the transcript windowing proof after the rebase onto the latest base', 'Drafting notes']
const layouts: readonly RowLayout[] = ['SR2-A', 'SR2-B', 'SR2-C']
const child = (parent: string, index: number): Row => ({
  ref: `${parent}/subagent/${index}`, id: `${parent}-${index}`, parentRef: parent, title: `Check output ${index + 1}`, status: 'idle', statusLabel: 'Idle', host: 'host-1', freshness: 'live',
  usage: { _tag: 'Unknown' }, duration: { _tag: 'Unknown' }, lastTurn: { _tag: 'Unknown' }, children: [],
})
/** Deterministic 100-row roster: title, work, reported facts, counts and subagents vary so every fitting branch runs. */
const roster = Array.from({ length: 100 }, (_, index) => {
  const ref = `agent/roster-${index}`
  const row: Row = {
    ref, id: `roster-${index}`, title: titles[index % titles.length]!, description: work[index % work.length],
    status: index % 5 === 0 ? 'waiting' : index % 3 === 0 ? 'idle' : 'working', statusLabel: index % 5 === 0 ? 'Needs you' : index % 3 === 0 ? 'Idle' : 'Working',
    host: `host-${(index % 4) + 1}`, harness: 'seat-1', model: index % 2 === 0 ? 'glm-5' : undefined,
    statusSince: storyNow - (index + 1) * 61_000, lastActivityAt: storyNow - (index + 1) * 13_000,
    needsMe: index % 5 === 0, unread: index % 4 === 0 ? (index % 13) + 1 : 0, freshness: index % 9 === 0 ? 'stale' : 'live',
    usage: index % 7 === 3 ? { _tag: 'Unknown' } : { _tag: 'Known', scope: '24h-root-and-subagents', usd: (index * 37 % 2900) / 100, tokens: (index * 7919 % 2_400_000) + 900 },
    duration: index % 6 === 4 ? { _tag: 'Unknown' } : { _tag: 'Known', scope: '24h-activity-span', ms: (index + 1) * 397_000 },
    lastTurn: index % 8 === 5 ? { _tag: 'Unknown' } : { _tag: 'Known', kind: 'turn-completed', at: storyNow - (index + 1) * 47_000 },
    branch: index % 3 === 1 ? `feat/roster-${index}` : undefined,
    pullRequest: index % 6 === 1 ? { ref: `${ref}/pr`, title: 'Roster fitting', number: 100 + index, state: 'open' } : undefined,
    children: Array.from({ length: index % 5 === 2 ? (index % 11) + 1 : 0 }, (_, childIndex) => child(ref, childIndex)),
  }
  return { row, layout: layouts[index % layouts.length]! }
})
function RosterRender() {
  const [mounted, setMounted] = React.useState(false)
  return <main data-roster-perf {...stylex.props(styles.root)}><Button isDisabled={mounted} onPress={() => { performance.mark('sidebar-roster-render-start'); setMounted(true) }}>Render 100-agent roster</Button><div data-frame-rows {...stylex.props(styles.rows)}>{mounted && roster.map(({ row, layout }) => <SidebarAgentRow key={row.ref} item={row} layout={layout} now={storyNow} />)}</div></main>
}
const meta = { title: 'Fractal UI/Sidebar/Roster', component: RosterRender, parameters: { layout: 'fullscreen' } } satisfies Meta<typeof RosterRender>
export default meta
type Story = StoryObj<typeof meta>

/** Observe the actual render task and its first 1.5s of fitting, not Storybook module/bootstrap work. */
export const HundredAgentRender: Story = { render: () => <RosterRender />, play: async ({ canvasElement }) => {
  await document.fonts.ready
  await expect(PerformanceObserver.supportedEntryTypes).toContain('longtask')
  performance.clearMarks('sidebar-roster-render-start')
  const tasks: { startTime: number; duration: number }[] = []
  const record = (entries: PerformanceEntry[]) => { for (const entry of entries) tasks.push({ startTime: entry.startTime, duration: entry.duration }) }
  const observer = new PerformanceObserver(list => record(list.getEntries()))
  observer.observe({ type: 'longtask' })
  await userEvent.click(within(canvasElement).getByRole('button', { name: 'Render 100-agent roster' }))
  await new Promise<void>(resolve => setTimeout(resolve, 1500))
  record(observer.takeRecords())
  observer.disconnect()
  const start = performance.getEntriesByName('sidebar-roster-render-start').at(-1)!.startTime
  const renderTasks = tasks.filter(task => task.startTime + task.duration >= start && task.startTime < start + 1500)
  const maxTaskMs = Math.max(0, ...renderTasks.map(task => task.duration))
  const root = canvasElement.querySelector<HTMLElement>('[data-roster-perf]')!
  root.dataset.rosterMeasurement = JSON.stringify({ rows: roster.length, start, windowMs: 1500, maxTaskMs, tasks: renderTasks })
  await expect(within(canvasElement).getAllByTestId('taste-agent-row')).toHaveLength(100)
  // This mixed cohort used to drop row 91's duration scope against a transient wide peer track and never restore it.
  const retained = canvasElement.querySelector<HTMLElement>('[data-wf-agent-ref="agent/roster-91"] [data-testid="taste-agent-row"]')!
  const scope = retained.querySelector<HTMLElement>('[data-row-column="time"] [data-line1-drop="scope"]')!
  await expect(scope.style.display).not.toBe('none')
  await expect(retained.querySelector<HTMLElement>('[data-row-column="time"]')!.clientWidth).toBeGreaterThanOrEqual(Math.ceil(Number.parseFloat(retained.dataset.rowTimeNatural!)))
  await expect(retained.querySelector<HTMLElement>('[data-row-column="title-text"]')!.clientWidth).toBeGreaterThanOrEqual(retained.clientWidth * 0.6)
} }
const styles = stylex.create({
  root: { height: '100vh', overflowY: 'auto', boxSizing: 'border-box', padding: s.lg, backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading },
  rows: { display: 'flex', flexDirection: 'column', gap: s.xs, width: g.sidebarDefault, minWidth: 0 },
})
