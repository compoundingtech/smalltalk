/** Baseline SR-2 row coverage: known-only hover card facts, omitted unreported fields, an aligned known-vs-none time cohort, and the row hover quick Open action. */
import * as React from 'react'
import type { Meta, StoryObj } from '@storybook/react-vite'
import * as stylex from '@stylexjs/stylex'
import { expect, userEvent, within } from 'storybook/test'
import { AgentHoverCard, AgentRowDetails, SidebarAgentRow } from './SidebarAgentRow'
import type { SidebarAgentRow as Row } from './model'
import { lightTheme } from '../composition-theme'
import { spaceVars as s, surfaceVars as surface, textVars as ink, typeVars as t, geometryVars as g, borderVars as border, radiusVars as r, elevationVars as elevation } from '../composition-tokens.stylex'

const storyNow = Date.UTC(2026, 9, 8, 12, 0, 0)
/** Fully reported synthetic identity; every optional fact the card can render is present. */
const knownRow: Row = {
  ref: 'agent/sample', id: 'sample', title: 'Sample session', description: 'Reviews the selection model',
  status: 'working', statusLabel: 'Working', host: 'host-1', harness: 'seat-1', model: 'glm-5',
  statusSince: storyNow - 90_000, lastActivityAt: storyNow - 30_000, needsMe: true, unread: 2, freshness: 'live',
  usage: { _tag: 'Known', scope: '24h-root-and-subagents', usd: 1.84, tokens: 412_000 },
  duration: { _tag: 'Known', scope: '24h-activity-span', ms: 3_600_000 },
  lastTurn: { _tag: 'Known', kind: 'turn-completed', at: storyNow - 300_000 },
  branch: 'feat/rows', worktree: '~/rows',
  pullRequest: { ref: 'agent/sample/pr', title: 'Align row cohort', number: 42, state: 'open' },
  children: [],
}
/** Same identity, nothing reported beyond title, status, host and counts; every optional fact is omitted. */
const unreportedRow: Row = { ...knownRow, ref: 'agent/unreported', id: 'unreported', title: 'Unreported session', usage: { _tag: 'Unknown' }, duration: { _tag: 'Unknown' }, lastTurn: { _tag: 'Unknown' }, description: undefined, model: undefined, harness: undefined, pullRequest: undefined, branch: undefined, worktree: undefined, terminal: undefined, mission: undefined, statusSince: undefined, lastActivityAt: undefined }
/** Attention and unread counts never reported; model and branch reported without harness, worktree or pull request. */
const uncountedRow: Row = { ...unreportedRow, ref: 'agent/uncounted', id: 'uncounted', title: 'Uncounted session', needsMe: undefined, unread: undefined, model: 'glm-5', branch: 'feat/rows' }
/** Synthetic cohort: every second row carries no last-turn fact, so its time slot renders empty while the shared metric track keeps every title aligned. */
const cohortRows: readonly Row[] = [
  { ...knownRow, ref: 'agent/first', id: 'first', title: 'First session' },
  { ...knownRow, ref: 'agent/second', id: 'second', title: 'Second session', lastTurn: { _tag: 'Unknown' }, needsMe: false, unread: 0 },
  { ...knownRow, ref: 'agent/third', id: 'third', title: 'Third session', status: 'waiting', statusLabel: 'Needs you' },
  { ...knownRow, ref: 'agent/fourth', id: 'fourth', title: 'Fourth session', lastTurn: { _tag: 'Unknown' }, usage: { _tag: 'Unknown' } },
  { ...unreportedRow, ref: 'agent/fifth', id: 'fifth', title: 'Fifth session' },
  { ...knownRow, ref: 'agent/sample/subagent/child', id: 'sample', parentRef: 'agent/sample', title: 'Check output', lastTurn: { _tag: 'Unknown' }, unread: undefined, description: undefined },
]

function CardPair({ row }: { readonly row: Row }) {
  return <div {...stylex.props(styles.spread)}>
    <div data-frame-rows {...stylex.props(styles.list)}><SidebarAgentRow item={row} now={storyNow} active /></div>
    <div {...stylex.props(styles.aside)}>
      <div data-testid="hover-facts"><AgentHoverCard row={row} now={storyNow} /></div>
    </div>
  </div>
}

async function assertCompactCard(canvasElement: HTMLElement) {
  const cards = within(canvasElement).getAllByTestId('agent-hover-card')
  await expect(cards).toHaveLength(1)
  const card = cards[0]!
  await expect(card.getBoundingClientRect().width).toBeLessThanOrEqual(400)
  const labels = [...card.querySelectorAll('dt')].map(label => label.textContent)
  await expect(new Set(labels).size).toBe(labels.length)
  for (const label of ['Name', 'ID', 'Description', 'Last activity', 'Worktree', 'Lease expires']) await expect(labels).not.toContain(label)
  await expect(card.querySelectorAll('li').length).toBeLessThanOrEqual(3)
  await expect(card.textContent).not.toMatch(/\d{4}-\d{2}-\d{2}T\d{2}:/)
}
function CohortList() {
  return <div data-frame-rows {...stylex.props(styles.list)}>{cohortRows.map((row, index) => <SidebarAgentRow key={row.ref} item={row} now={storyNow} active={index === 0} />)}</div>
}

function QuickOpenCohort() {
  const [openedRef, setOpenedRef] = React.useState<string | undefined>()
  const open = React.useCallback((row: Row) => setOpenedRef(row.ref), [])
  const select = React.useCallback((id: string) => setOpenedRef(`agent/${id}`), [])
  return <div data-frame-rows {...stylex.props(styles.list)}>{cohortRows.slice(0, 3).map(row => <SidebarAgentRow key={row.ref} item={row} now={storyNow} actions={{ select }} onOpen={open} active={openedRef === row.ref} />)}</div>
}
function DetailsPair({ row }: { readonly row: Row }) {
  return <div data-testid={`details-${row.id}`} {...stylex.props(styles.spread)}>
    <div data-frame-rows {...stylex.props(styles.list)}><SidebarAgentRow item={row} now={storyNow} extraSignals={['X-needs', 'X-unread', 'X-model', 'X-pr']} onOpen={() => undefined} /></div>
    <div {...stylex.props(styles.aside)}><AgentRowDetails row={row} now={storyNow} /></div>
  </div>
}
/** The open row button's description: its name is the visible content plus name facts, the remaining reported facts arrive by `aria-describedby`. */
const rowDescription = (pair: HTMLElement) => {
  const button = pair.querySelector<HTMLElement>('[data-testid="taste-agent-row"] button[aria-describedby]')
  return button === null ? null : pair.ownerDocument.getElementById(button.getAttribute('aria-describedby')!)?.textContent ?? null
}
/** Every rendered label, value, title, accessible name and description omits facts the source never reported. */
async function assertCountsOmitted(pair: HTMLElement) {
  const labels = [...pair.querySelectorAll('dt')].map(label => label.textContent)
  for (const label of ['Needs you', 'Unread']) await expect(labels, `${label} rendered without a reported value`).not.toContain(label)
  await expect(pair.textContent).not.toMatch(/unknown/i)
  for (const element of pair.querySelectorAll('[title], [aria-label], [aria-description]')) await expect(['title', 'aria-label', 'aria-description'].map(name => element.getAttribute(name) ?? '').join(' ')).not.toMatch(/unknown|not reported/i)
  await expect(rowDescription(pair), 'row description').toMatch(/(?:^|\. )Host: host-1\. Observation: live\./)
  await expect(rowDescription(pair)).not.toMatch(/unread|attention/i)
}

const meta = {
  title: 'Fractal UI/Sidebar/Agent Row',
  component: SidebarAgentRow,
  parameters: { layout: 'fullscreen', docs: { description: { component: 'Baseline agent row: reported facts only. The hover card and reported details omit unreported fields instead of showing placeholders; hovering a row swaps the time slot for a quick Open action; rows without a reported time keep the shared metric track so titles stay aligned.' } } },
} satisfies Meta
export default meta
type Story = StoryObj
export const HoverCardKnown: Story = { name: 'Hover card · fully reported', render: () => <main {...stylex.props(styles.root)}><CardPair row={knownRow} /></main>, play: async ({ canvasElement }) => assertCompactCard(canvasElement) }
export const HoverCardUnknown: Story = { name: 'Hover card · unreported fields omitted', render: () => <main {...stylex.props(styles.root)}><CardPair row={unreportedRow} /></main>, play: async ({ canvasElement }) => {
  const card = within(canvasElement).getByTestId('hover-facts')
  await assertCompactCard(canvasElement)
  await expect(card.textContent).not.toMatch(/unknown|unavailable|—/i)
  for (const label of ['Spend', 'Duration', 'Last turn', 'Model', 'PR', 'Branch']) await expect(within(card).queryByText(label)).toBeNull()
} }
export const CountsUnreported: Story = { name: 'Details · attention and unread unreported', render: () => <main {...stylex.props(styles.root)}><DetailsPair row={uncountedRow} /><DetailsPair row={knownRow} /></main>, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  const uncounted = canvas.getByTestId('details-uncounted')
  await assertCountsOmitted(uncounted)
  await expect(within(uncounted).getByTitle('model: glm-5')).toBeInTheDocument()
  await expect(within(uncounted).getByTitle('branch: feat/rows')).toBeInTheDocument()
  // Control: reported counts render as pairs, so the omission check must fail on them. The button names them, so its description does not repeat them.
  const known = canvas.getByTestId('details-sample')
  const labels = [...known.querySelectorAll('dt')].map(label => label.textContent)
  await expect(labels).toEqual(expect.arrayContaining(['Needs you', 'Unread']))
  await expect(within(known).getByRole('button', { name: /^Sample session \(Working, needs your attention, 2 unread\) / })).toBeEnabled()
  await expect(rowDescription(known)).not.toMatch(/attention|unread/i)
  await expect(assertCountsOmitted(known)).rejects.toThrow()
} }
export const CountsUnreportedLight: Story = { ...CountsUnreported, name: 'Details · attention and unread unreported (light)', render: () => <main {...stylex.props(styles.root, lightTheme)}><DetailsPair row={uncountedRow} /><DetailsPair row={knownRow} /></main> }
export const TimeCohort: Story = { name: 'Time cohort · known vs none', render: () => <main {...stylex.props(styles.root)}><CohortList /></main>, play: async ({ canvasElement }) => {
  const rows = within(canvasElement).getAllByTestId('taste-agent-row')
  const child = rows.find(row => row.textContent?.includes('Check output'))!
  await expect(child).toBeDefined()
  await expect(child.querySelector('[data-row-field="last-turn"]')).toBeNull()
  const titleLeft = rows[0]!.querySelector('[data-row-column="title-text"]')!.getBoundingClientRect().left
  for (const row of rows) await expect(Math.abs(row.querySelector('[data-row-column="title-text"]')!.getBoundingClientRect().left - titleLeft)).toBeLessThanOrEqual(1)
} }
export const QuickOpenAction: Story = { name: 'Row hover · quick Open', render: () => <main {...stylex.props(styles.root)}><QuickOpenCohort /></main>, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  const rows = canvas.getAllByTestId('taste-agent-row')
  const target = rows[1]!
  await document.fonts.ready
  await new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve())))
  await userEvent.hover(target)
  await userEvent.click(within(target).getByRole('button', { name: /^Open / }))
  await expect(target.querySelector('[aria-current="page"]')).not.toBeNull()
} }
export const AllStates: Story = { render: () => <main {...stylex.props(styles.root)}><CardPair row={knownRow} /><CardPair row={unreportedRow} /><CohortList /></main> }
/** One row per name shape: SG-1 layouts name the glyph and badges, SG-2 and SR2-C already show the status inside the button. */
const nameShapes = {
  default: { variant: 'SR-2', layout: 'SR2-A', glyph: 'SG-1', row: knownRow },
  'sr2-b': { variant: 'SR-2', layout: 'SR2-B', glyph: 'SG-1', row: knownRow },
  'sr2-c': { variant: 'SR-2', layout: 'SR2-C', glyph: 'SG-1', row: knownRow },
  'sr-3': { variant: 'SR-3', layout: 'SR2-A', glyph: 'SG-1', row: knownRow },
  compact: { variant: 'SR-1', layout: 'SR2-A', glyph: 'SG-1', row: knownRow },
  'sg-2': { variant: 'SR-1', layout: 'SR2-A', glyph: 'SG-2', row: knownRow },
  'sr-3-sg-2': { variant: 'SR-3', layout: 'SR2-A', glyph: 'SG-2', row: knownRow },
  uncounted: { variant: 'SR-1', layout: 'SR2-A', glyph: 'SG-1', row: uncountedRow },
} as const
/** Exact computed names for the fixture rows; Playwright's snapshot agrees in Chromium. */
const expectedNames: Record<keyof typeof nameShapes, string> = {
  default: 'Sample session (Working, needs your attention, 2 unread) Reviews the selection model Host host-1 24h root and subagents: $1.84 24h root and subagents: 412000 tokens 24h activity span: 1h',
  'sr2-b': 'Sample session (Working, needs your attention, 2 unread) Reviews the selection model Host host-1 Last completed turn: 5m ago',
  'sr2-c': 'Sample session (needs your attention, 2 unread) Working Reviews the selection model Host host-1 24h activity span: 1h Last completed turn: 5m ago',
  'sr-3': 'Sample session (Working, needs your attention, 2 unread) Reviews the selection model Host host-1 24h root and subagents: $1.84 24h root and subagents: 412000 tokens 24h activity span: 1h Reviews the selection model / seat-1 / glm-5 / feat/rows / ~/rows / PR #42',
  compact: 'Sample session (Working, needs your attention, 2 unread)',
  'sg-2': 'Working Sample session (needs your attention, 2 unread)',
  'sr-3-sg-2': 'Working Sample session (needs your attention, 2 unread) Reviews the selection model Host host-1 24h root and subagents: $1.84 24h root and subagents: 412000 tokens 24h activity span: 1h Reviews the selection model / seat-1 / glm-5 / feat/rows / ~/rows / PR #42',
  uncounted: 'Uncounted session (Working)',
}
function NameRows() {
  return <div data-frame-rows {...stylex.props(styles.list)}>
    {Object.entries(nameShapes).map(([key, shape]) => <div key={key} data-testid={`name-${key}`}><SidebarAgentRow item={shape.row} variant={shape.variant} layout={shape.layout} glyph={shape.glyph} now={storyNow} onOpen={() => undefined} /></div>)}
  </div>
}
const occurrences = (text: string, fact: string) => text.split(fact).length - 1
/** The name says the title and status once, plus attention and unread when reported; the description repeats none of them. */
async function assertNamed(container: HTMLElement, key: keyof typeof nameShapes) {
  const row = nameShapes[key].row
  const button = within(container).getByRole('button', { name: expectedNames[key] })
  await expect(button).toBeEnabled()
  await expect(occurrences(expectedNames[key], row.statusLabel), `${key} status in name`).toBe(1)
  const description = rowDescription(container)!
  await expect(description, `${key} description`).toMatch(/\S/)
  for (const fact of [row.title, row.statusLabel, 'attention', 'unread', 'Unread']) await expect(description, `${key} description repeats ${fact}`).not.toContain(fact)
}
export const AccessibleName: Story = { name: 'Row name · status and badges', render: () => <main {...stylex.props(styles.root)}><NameRows /></main>, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  const keys = Object.keys(nameShapes) as (keyof typeof nameShapes)[]
  for (const key of keys) await assertNamed(canvas.getByTestId(`name-${key}`), key)
  await expect(canvas.queryAllByRole('button', { name: /unknown|not reported/i })).toHaveLength(0)
  for (const key of keys) {
    const container = canvas.getByTestId(`name-${key}`)
    const facts = container.querySelector<HTMLElement>('[data-row-name-facts]')!
    const describedBy = container.ownerDocument.getElementById(container.querySelector('button[aria-describedby]')!.getAttribute('aria-describedby')!)!
    const description = describedBy.textContent
    // Control: the glyph-only name (facts hidden) must fail the exact name.
    facts.hidden = true
    await expect(assertNamed(container, key), `${key} name control`).rejects.toThrow()
    facts.hidden = false
    // Control: the status always prepended to the facts (the shape before SG-2/SR2-C suppression) must fail.
    const text = facts.textContent!
    facts.textContent = `(${nameShapes[key].row.statusLabel}, ${text.slice(1)}`
    await expect(assertNamed(container, key), `${key} status control`).rejects.toThrow()
    facts.textContent = text
    // Control: the full metadata as description repeats the name facts and must fail.
    describedBy.textContent = container.querySelector('[data-row-column="title"]')!.getAttribute('title')
    await expect(assertNamed(container, key), `${key} description control`).rejects.toThrow()
    describedBy.textContent = description
    await assertNamed(container, key)
  }
} }
export const AccessibleNameLight: Story = { ...AccessibleName, name: 'Row name · status and badges (light)', render: () => <main {...stylex.props(styles.root, lightTheme)}><NameRows /></main> }

const styles = stylex.create({
  root: { height: '100vh', minHeight: 0, overflowY: 'auto', boxSizing: 'border-box', padding: s.lg, backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading, display: 'flex', flexDirection: 'column', gap: s.xl },
  spread: { display: 'flex', alignItems: 'flex-start', flexWrap: 'wrap', gap: s.xl },
  list: { display: 'flex', flexDirection: 'column', gap: s.xs, width: g.specimenAside, minWidth: 0 },
  aside: { width: g.specimenAside, maxWidth: '100%', minWidth: 0, boxSizing: 'border-box', padding: s.lg, backgroundColor: surface.raised, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.md, boxShadow: elevation.popover },
})
