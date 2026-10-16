import * as React from 'react'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { expect, userEvent, waitFor, within } from 'storybook/test'
import * as Atom from 'effect/reactivity/Atom'
import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import { DataSourceProvider } from '../data/react.tsx'
import { fixtureSource } from '../data/fixtureSource.ts'
import { observed, waiting } from '../data/source.ts'
import { fixtureProjections } from '../fixtures/projections.ts'
import { LiveAgentWorkspace, narrowLayoutQuery } from './LiveAgentWorkspace.tsx'

// The real workspace over the deterministic fixture world, with resources supported as on the live gateway.
const source = {
  ...fixtureSource({ world: fixtureProjections }),
  resources: { byAgent: () => Atom.make(observed({ value: { items: [], nextCursor: null } })), byId: () => Atom.make(waiting), subjects: Atom.make([]), loadMore: () => undefined, refresh: () => undefined, resolveFile: () => undefined },
}
const storyAgent = 'agent/build-host-a/workbench-shell'

const NarrowLayoutStory = (_: { readonly scheme: 'dark' | 'light' }) => {
  const [registry] = React.useState(() => AtomRegistry.make())
  return <DataSourceProvider source={source} registry={registry}><LiveAgentWorkspace /></DataSourceProvider>
}

/** Below 900 px the thread is the only thing on screen; one Agent details button opens the agent's actions and Details as a sheet. Wider windows keep the inline panel. */
const meta = {
  title: 'Fractal/Narrow layout',
  component: NarrowLayoutStory,
  parameters: { layout: 'fullscreen' },
  args: { scheme: 'dark' },
  argTypes: { scheme: { options: ['dark', 'light'], control: 'radio' } },
  // The workspace keeps its scheme, agent and panes in persisted UI state; every story starts from the same state.
  // A storage event makes the already-loaded persistence merge it, as it does for another tab's write.
  beforeEach: ({ args }) => {
    const entry = (value: unknown) => ({ version: 1, value, clock: Date.now(), writer: 'narrow-layout-story' })
    const raw = JSON.stringify({ version: 1, entries: { 'round2.scheme': entry(args.scheme), 'round2.agent': entry(storyAgent), 'round2.sidebarClosed': entry(false), [`round2.panes.${storyAgent}`]: entry([]) } })
    window.localStorage.setItem('wf.ui@1', raw)
    window.dispatchEvent(new StorageEvent('storage', { key: 'wf.ui@1', newValue: raw }))
  },
} satisfies Meta<typeof NarrowLayoutStory>
export default meta
type Story = StoryObj<typeof meta>

const workspace = async (canvasElement: HTMLElement) => {
  await waitFor(() => expect(within(canvasElement).getByText('Let me find where groups close.')).toBeVisible(), { timeout: 60000 })
  const narrow = window.matchMedia(narrowLayoutQuery).matches
  return { canvas: within(canvasElement), narrow, header: within(within(canvasElement).getByTestId('thread-header')) }
}
/** Wide windows keep the docked sidebar, the full header cluster and the inline Details panel. */
const assertWide = async (canvasElement: HTMLElement) => {
  const { canvas, header } = await workspace(canvasElement)
  await expect(canvas.getByRole('complementary', { name: 'Agents' })).toBeVisible()
  await expect(canvas.queryByRole('dialog', { name: 'Agents' })).toBeNull()
  await expect(header.queryByRole('button', { name: 'Expand sidebar' })).toBeNull()
  await expect(header.getByRole('button', { name: 'Toggle right panel' })).toBeVisible()
  await expect(header.getByRole('button', { name: 'Agent actions for WorkbenchShell' })).toBeVisible()
  await expect(header.getByText('build-host-a')).toBeVisible()
  await expect(header.queryByRole('button', { name: 'Agent details' })).toBeNull()
}
const openWideDetails = async (canvasElement: HTMLElement) => {
  await assertWide(canvasElement)
  const { canvas, header } = await workspace(canvasElement)
  await userEvent.click(header.getByRole('button', { name: 'Toggle right panel' }))
  await expect(await canvas.findByRole('complementary', { name: 'Changes' })).toBeVisible()
  await expect(canvas.queryByRole('dialog', { name: 'Agent details' })).toBeNull()
}

/** The thread fills the window; the header keeps the chevron, host / agent breadcrumb, status and one Agent details button. */
export const Drawers: Story = { play: async ({ canvasElement }) => {
  const { canvas, narrow, header } = await workspace(canvasElement)
  if (!narrow) return assertWide(canvasElement)
  // The sidebar is not in the layout until opened.
  await expect(canvas.queryByRole('complementary', { name: 'Agents' })).toBeNull()
  await expect(canvas.queryByRole('navigation', { name: 'Agent roster' })).toBeNull()
  await expect(Math.abs(canvas.getByRole('region', { name: 'Agent workspace' }).getBoundingClientRect().width - window.innerWidth)).toBeLessThanOrEqual(1)
  await expect(header.getByRole('button', { name: 'Expand sidebar' })).toBeVisible()
  const opener = header.getByRole('button', { name: 'Agent details' })
  await expect(opener).toHaveAttribute('aria-expanded', 'false')
  await expect(opener).toHaveAttribute('aria-haspopup', 'dialog')
  const breadcrumb = header.getByRole('navigation', { name: 'Breadcrumb' })
  await expect(breadcrumb).toHaveTextContent('build-host-a')
  await expect(breadcrumb).toHaveTextContent('WorkbenchShell')
  await expect(breadcrumb.getBoundingClientRect().right).toBeLessThanOrEqual(opener.getBoundingClientRect().left)
  for (const name of ['Agent actions for WorkbenchShell', 'Resources', 'Conversation menu', 'Toggle terminal drawer', 'Toggle right panel']) await expect(header.queryByRole('button', { name })).toBeNull()
} }
export const DrawersLight: Story = { ...Drawers, args: { scheme: 'light' } }

const openDetailsSheet = async (canvasElement: HTMLElement) => {
  const { canvas, header } = await workspace(canvasElement)
  const opener = header.getByRole('button', { name: 'Agent details' })
  await userEvent.click(opener)
  const sheet = await canvas.findByRole('dialog', { name: 'Agent details' })
  // Changes is a lazy chunk; on a cold dev server its first load takes longer than findBy's default second.
  await expect(await within(sheet).findByRole('complementary', { name: 'Changes' }, { timeout: 15000 })).toBeInTheDocument()
  await waitFor(() => expect(sheet.contains(document.activeElement)).toBe(true))
  return { canvas, sheet, opener }
}
/** The right sheet: the agent's actions in one row above Changes, which scrolls as one column. */
export const DrawersOpen: Story = { play: async ({ canvasElement }) => {
  const { narrow } = await workspace(canvasElement)
  if (!narrow) return openWideDetails(canvasElement)
  const { sheet, opener } = await openDetailsSheet(canvasElement)
  await expect(opener).toHaveAttribute('aria-expanded', 'true')
  const actions = within(sheet).getByRole('toolbar', { name: 'Agent actions' })
  for (const name of ['Agent actions for WorkbenchShell', 'Resources', 'Conversation menu', 'Terminal']) await expect(within(actions).getByRole('button', { name })).toBeInTheDocument()
  await expect(within(sheet).getByRole('button', { name: 'Close agent details' })).toBeVisible()
  await expect(canvasElement.querySelector('[data-testid="live-agent-workspace"] > aside[aria-label="Changes"]')).toBeNull()
  // The captured-change rows sit above the 'Captured change' header, never under it.
  const header = await within(sheet).findByRole('heading', { name: 'Captured change' })
  const rows = [...sheet.querySelectorAll('[data-captured-change]')]
  await expect(rows.length).toBeGreaterThan(0)
  const headerTop = header.getBoundingClientRect().top
  for (const row of rows) await expect(row.getBoundingClientRect().bottom).toBeLessThanOrEqual(headerTop)
  const changes = within(sheet).getByRole('complementary', { name: 'Changes' })
  await expect(changes.getBoundingClientRect().width).toBeLessThanOrEqual(sheet.getBoundingClientRect().width)
} }
/** Keyboard path through both sheets: each opens with focus inside, Escape or the backdrop closes it, and focus returns to its opener. */
export const DrawersFlow: Story = { play: async ({ canvasElement }) => {
  const { canvas, narrow, header } = await workspace(canvasElement)
  if (!narrow) return assertWide(canvasElement)
  // Agents, from the left.
  const chevron = header.getByRole('button', { name: 'Expand sidebar' })
  await expect(chevron).toHaveAttribute('aria-haspopup', 'dialog')
  await expect(chevron).toHaveAttribute('aria-expanded', 'false')
  await userEvent.hover(chevron)
  chevron.focus()
  await userEvent.keyboard('{Enter}')
  const agents = await canvas.findByRole('dialog', { name: 'Agents' })
  await expect(within(agents).getByRole('navigation', { name: 'Agent roster' })).toBeVisible()
  await expect(agents.getBoundingClientRect().left).toBe(0)
  await waitFor(() => expect(agents.contains(document.activeElement)).toBe(true))
  await expect(chevron).toHaveAttribute('aria-expanded', 'true')
  await expect(agents.id).not.toBe('')
  await expect(chevron).toHaveAttribute('aria-controls', agents.id)
  await userEvent.keyboard('{Escape}')
  await waitFor(() => expect(canvas.queryByRole('dialog', { name: 'Agents' })).toBeNull())
  await waitFor(() => expect(chevron).toHaveFocus())
  await expect(chevron).toHaveAttribute('aria-expanded', 'false')
  await expect(chevron).not.toHaveAttribute('aria-controls')
  await userEvent.keyboard('{Enter}')
  const backdrop = (await canvas.findByRole('dialog', { name: 'Agents' })).parentElement!.parentElement!
  await userEvent.click(backdrop)
  await waitFor(() => expect(canvas.queryByRole('dialog', { name: 'Agents' })).toBeNull())
  await waitFor(() => expect(chevron).toHaveFocus())
  // Choosing an agent closes the sheet on its thread.
  await userEvent.keyboard('{Enter}')
  await userEvent.click(within(await canvas.findByRole('dialog', { name: 'Agents' })).getByRole('button', { name: 'MissionsUi' }))
  await waitFor(() => expect(canvas.queryByRole('dialog', { name: 'Agents' })).toBeNull())
  await waitFor(() => expect(header.getByRole('navigation', { name: 'Breadcrumb' })).toHaveTextContent('MissionsUi'))
  // Agent details, from the right.
  const opener = header.getByRole('button', { name: 'Agent details' })
  await userEvent.hover(opener)
  opener.focus()
  await userEvent.keyboard('{Enter}')
  const sheet = await canvas.findByRole('dialog', { name: 'Agent details' })
  await waitFor(() => expect(sheet.contains(document.activeElement)).toBe(true))
  await expect(opener).toHaveAttribute('aria-expanded', 'true')
  await userEvent.tab()
  await expect(sheet.contains(document.activeElement)).toBe(true)
  await userEvent.keyboard('{Escape}')
  await waitFor(() => expect(canvas.queryByRole('dialog', { name: 'Agent details' })).toBeNull())
  await waitFor(() => expect(opener).toHaveFocus())
  await expect(opener).toHaveAttribute('aria-expanded', 'false')
  // The close button is the pointer path out.
  await userEvent.keyboard('{Enter}')
  const reopened = await canvas.findByRole('dialog', { name: 'Agent details' })
  await userEvent.click(within(reopened).getByRole('button', { name: 'Close agent details' }))
  await waitFor(() => expect(canvas.queryByRole('dialog', { name: 'Agent details' })).toBeNull())
} }
