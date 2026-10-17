import * as React from 'react'
import * as Aria from 'react-aria-components'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react'
import { expect, userEvent, waitFor, within } from 'storybook/test'
import { SidebarAgentRow, ThreadHeader, assistantDarkTheme, compositionLightTheme, type SidebarAgentRowData } from '@smalltalk/fractal-ui/assistant-ui/shell'
import { colorVars as c, typeVars as t, spaceVars as s } from '../../../../packages/fractal-ui/src/assistant-ui/composition-tokens.stylex.ts'
import { AgentFolders } from '../shell/AgentFolders.tsx'
import type { SidebarNode } from '../folders/sidebarContract.ts'
import { ResponsiveAgentSidebar, sidebarDrawerBreakpoint, sidebarMinimumWidth, useSidebarShortcut } from './ResponsiveAgentSidebar.tsx'

const storyNow = Date.UTC(2026, 9, 8, 12)
const rows: readonly SidebarAgentRowData[] = ['Builder', 'Reviewer'].map(title => ({
  ref: `agent/${title.toLowerCase()}`, id: `agent/${title.toLowerCase()}`, title: `${title} session`, host: 'local', status: 'idle', statusLabel: 'Idle', freshness: 'live',
  children: [], usage: { _tag: 'Known', scope: '24h-root-and-subagents', usd: 4.2, tokens: 2_300_000 }, duration: { _tag: 'Known', scope: '24h-activity-span', ms: 3_600_000 }, lastTurn: { _tag: 'Known', kind: 'turn-completed', at: storyNow - 180_000 },
}))
const tree: readonly SidebarNode[] = [{ _tag: 'Folder', id: 'product', label: 'Product', collapsed: false, children: rows.map(row => ({ _tag: 'Agent', id: row.ref, subject: row.ref, label: row.title })) }]

/** Fixture data surrounds the same sidebar frame, tree, row and thread header as live. */
const ResponsiveSidebarSurface = ({ viewport, scheme = 'dark', shortcut = true, range = 'resizable' }: { readonly viewport: number; readonly scheme?: 'light' | 'dark'; readonly shortcut?: boolean; readonly range?: 'resizable' | 'fixed' | 'reversed' | 'nonfinite' }) => {
  const [collapsed, setCollapsed] = React.useState(false)
  const [drawerOpen, setDrawerOpen] = React.useState(false)
  const [selected, setSelected] = React.useState(rows[0]!.ref)
  const maxWidth = range === 'fixed' ? sidebarMinimumWidth : range === 'reversed' ? sidebarMinimumWidth - 1 : range === 'nonfinite' ? Number.NaN : Math.max(sidebarMinimumWidth, Math.min(440, viewport - 640))
  const [width, setWidth] = React.useState(range === 'resizable' ? Math.min(maxWidth, Math.max(sidebarMinimumWidth, Math.round(viewport * 256 / 1440))) : sidebarMinimumWidth)
  const [commits, setCommits] = React.useState(0)
  const opener = React.useRef<HTMLButtonElement>(null)
  const narrow = viewport < sidebarDrawerBreakpoint
  const toggle = React.useCallback(() => {
    if (narrow) { opener.current?.focus(); setDrawerOpen(value => !value) }
    else setCollapsed(value => !value)
  }, [narrow])
  useSidebarShortcut(shortcut ? toggle : undefined)
  const select = (ref: string) => { setSelected(ref); setDrawerOpen(false) }
  return <main data-sidebar-layout-story="" data-viewport={viewport} data-scheme={scheme} data-range={range} data-resize-commits={commits} style={{ width: viewport }} {...stylex.props(styles.root, scheme === 'dark' ? assistantDarkTheme : compositionLightTheme)}>
    <ResponsiveAgentSidebar viewport={viewport} width={width} maxWidth={maxWidth} collapsed={collapsed} drawerOpen={drawerOpen} onDrawerOpenChange={setDrawerOpen} onToggleCollapsed={() => setCollapsed(value => !value)} onWidthChange={setWidth} onWidthCommit={value => { setWidth(value); setCommits(count => count + 1) }} onWidthReset={() => setWidth(Math.min(maxWidth, 256))}>
      <Aria.SearchField aria-label="Search agents" {...stylex.props(styles.search)}><Aria.Input placeholder="Search agents" {...stylex.props(styles.input)} /></Aria.SearchField>
      <nav aria-label="Agent roster" {...stylex.props(styles.roster)}>
        <AgentFolders tree={tree} canDrop={() => ({ refused: 'Read-only layout fixture' })} onMove={() => undefined} onCreateFolder={() => undefined} onRenameFolder={() => undefined} onDeleteFolder={() => undefined} onToggleCollapsed={() => undefined} onSelect={select} renderAgent={node => <SidebarAgentRow inTree item={rows.find(row => row.ref === node.id)!} now={storyNow} active={selected === node.id} onOpen={() => select(node.id)} />} />
      </nav>
    </ResponsiveAgentSidebar>
    <section aria-label="Agent workspace" data-sidebar-thread="" {...stylex.props(styles.thread)}>
      <ThreadHeader title={rows.find(row => row.ref === selected)!.title} folder="local" now={storyNow} sidebarCollapsed={narrow || collapsed} sidebarToggleRef={opener} onToggleSidebar={toggle} panelOpen={false} drawerOpen={false} onTogglePanel={() => undefined} onToggleDrawer={() => undefined} />
      <article {...stylex.props(styles.article)}><h1 {...stylex.props(styles.heading)}>Review the sidebar</h1><p>Opening navigation does not move this thread. The roster remains readable in a minimum 240px drawer.</p><Aria.Button {...stylex.props(styles.button)}>Reply to this session</Aria.Button></article>
    </section>
    {/* The last raster row belongs to fixed chrome, matching the deployed devbar boundary. */}
    <div aria-hidden="true" data-fixed-chrome-boundary="" {...stylex.props(styles.chromeBoundary)} />
  </main>
}

type LayoutShiftEntry = PerformanceEntry & { readonly value: number; readonly hadRecentInput: boolean; readonly sources: readonly { readonly node: Node | null }[] }
const assertModalIsolation = (dialog: HTMLElement, thread: HTMLElement): void => {
  if (dialog.getAttribute('aria-modal') !== 'true') throw new Error('Navigation dialog must declare aria-modal=true')
  if (thread.closest('[inert], [aria-hidden="true"]') === null) throw new Error('Workspace must be hidden or inert while navigation is open')
  if (dialog.closest('[inert], [aria-hidden="true"]') !== null) throw new Error('Navigation dialog must remain accessible')
}

const assertSidebarHitTarget = (splitter: HTMLElement): void => {
  const box = splitter.getBoundingClientRect()
  for (let y = box.top + 0.5; y < box.bottom; y += 1) {
    for (const x of [box.left - 2.5, box.left + box.width / 2, box.right + 2.5]) {
      const target = splitter.ownerDocument.elementFromPoint(x, y)
      if (target !== splitter && !splitter.contains(target)) throw new Error(`Sidebar resize hit intercepted at ${x}, ${y}`)
    }
  }
}

/** Synthetic handler controls model capture explicitly; native capture is proved on the live browser. */
const assertResizeLifecycle = async (splitter: HTMLElement, root: HTMLElement): Promise<void> => {
  const originalCapture = Object.getOwnPropertyDescriptor(splitter, 'setPointerCapture')
  const originalHasCapture = Object.getOwnPropertyDescriptor(splitter, 'hasPointerCapture')
  const originalRelease = Object.getOwnPropertyDescriptor(splitter, 'releasePointerCapture')
  let captured = false
  Object.defineProperties(splitter, {
    setPointerCapture: { configurable: true, value: () => { captured = true } },
    hasPointerCapture: { configurable: true, value: () => captured },
    releasePointerCapture: { configurable: true, value: () => { captured = false } },
  })
  const pointer = (type: string, x: number) => splitter.dispatchEvent(new PointerEvent(type, { bubbles: true, pointerId: 1, button: 0, clientX: x }))
  const value = () => Number(splitter.getAttribute('aria-valuenow'))
  try {
    const before = value(), commits = root.dataset.resizeCommits
    for (const cancellation of ['Escape', 'pointercancel', 'lostpointercapture']) {
      pointer('pointerdown', 100)
      pointer('pointermove', 116)
      await waitFor(() => expect(value()).toBe(before + 16))
      await expect(captured).toBe(true)
      if (cancellation === 'Escape') root.ownerDocument.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true }))
      else pointer(cancellation, 116)
      await waitFor(() => expect(value()).toBe(before))
      await expect(captured).toBe(false)
      pointer('pointermove', 132)
      pointer('pointerup', 132)
      await new Promise<void>(resolve => requestAnimationFrame(() => resolve()))
      await expect(value()).toBe(before)
      await expect(root.dataset.resizeCommits).toBe(commits)
    }
    // Pending RAF must also be cancelled when Escape arrives before the first paint.
    pointer('pointerdown', 100); pointer('pointermove', 116)
    root.ownerDocument.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true }))
    pointer('pointerup', 116)
    await new Promise<void>(resolve => requestAnimationFrame(() => resolve()))
    await expect(value()).toBe(before)
    await expect(root.dataset.resizeCommits).toBe(commits)
    // Negative control: an uncancelled release commits, so rollback is not a no-op drag.
    pointer('pointerdown', 100); pointer('pointermove', 116); pointer('pointerup', 116)
    await waitFor(() => expect(value()).toBe(before + 16))
    await expect(Number(root.dataset.resizeCommits)).toBe(Number(commits) + 1)
    splitter.focus()
    await userEvent.keyboard('{ArrowLeft}{ArrowLeft}')
    await waitFor(() => expect(value()).toBe(before))
  } finally {
    for (const [key, descriptor] of [['setPointerCapture', originalCapture], ['hasPointerCapture', originalHasCapture], ['releasePointerCapture', originalRelease]] as const) {
      if (descriptor === undefined) Reflect.deleteProperty(splitter, key)
      else Object.defineProperty(splitter, key, descriptor)
    }
  }
}
const drawerPlay = async ({ canvasElement }: { readonly canvasElement: HTMLElement }) => {
  await document.fonts.ready
  const root = canvasElement.querySelector<HTMLElement>('[data-sidebar-layout-story]')!
  const thread = root.querySelector<HTMLElement>('[data-sidebar-thread]')!
  const viewport = Number(root.dataset.viewport)
  const canvas = within(root)
  const rect = () => { const value = thread.getBoundingClientRect(); return { x: value.x, y: value.y, width: value.width, height: value.height } }
  if (viewport >= sidebarDrawerBreakpoint) {
    const sidebar = canvas.getByRole('complementary', { name: 'Agents' })
    await expect(sidebar.getBoundingClientRect().width).toBeGreaterThanOrEqual(sidebarMinimumWidth)
    await expect(root.querySelector('[role="dialog"]')).toBeNull()
    const splitter = canvas.getByRole('separator', { name: 'Agent sidebar width' })
    assertSidebarHitTarget(splitter)
    const previousStack = splitter.style.zIndex
    try {
      splitter.style.zIndex = '0'
      await expect(() => assertSidebarHitTarget(splitter)).toThrow('Sidebar resize hit intercepted')
    } finally { splitter.style.zIndex = previousStack }
    assertSidebarHitTarget(splitter)
    splitter.focus()
    await userEvent.keyboard('{Home}')
    await expect(sidebar.getBoundingClientRect().width).toBe(sidebarMinimumWidth)
    const track = root.querySelector<HTMLElement>('[data-resizer-track="live-agent-sidebar"]')!
    await expect(track.getBoundingClientRect().height - splitter.getBoundingClientRect().height).toBe(1)
    await waitFor(() => assertSidebarHitTarget(splitter))
    const originalStack = splitter.style.zIndex
    try {
      splitter.style.zIndex = '0'
      await expect(() => assertSidebarHitTarget(splitter)).toThrow('Sidebar resize hit intercepted')
    } finally { splitter.style.zIndex = originalStack }
    await waitFor(() => assertSidebarHitTarget(splitter))
    // Negative control: restore the old oversized bounds, including the fixed chrome's row.
    const originalBottom = splitter.style.bottom
    try {
      splitter.style.bottom = '0'
      await expect(() => assertSidebarHitTarget(splitter)).toThrow('Sidebar resize hit intercepted')
    } finally { splitter.style.bottom = originalBottom }
    await waitFor(() => assertSidebarHitTarget(splitter))
    await assertResizeLifecycle(splitter, root)
    root.dataset.drawerMeasurement = JSON.stringify({ viewport, presentation: 'dock', sidebarWidth: sidebar.getBoundingClientRect().width, thread: rect() })
    return
  }
  await expect(canvas.queryByRole('complementary', { name: 'Agents' })).toBeNull()
  await expect(thread.getBoundingClientRect().width).toBe(viewport)
  const opener = canvas.getByRole('button', { name: 'Expand sidebar' })
  await expect(opener.querySelector('[title]')).toBeNull()
  await userEvent.hover(opener)
  await expect(await within(document.body).findByRole('tooltip', { name: 'Expand sidebar (⌘/Ctrl+B)' })).toBeVisible()
  await userEvent.unhover(opener)
  const before = rect()
  const entries: { readonly value: number; readonly hadRecentInput: boolean }[] = []
  const record = (records: PerformanceEntry[]) => { for (const entry of records as LayoutShiftEntry[]) if (entry.sources.some(source => source.node !== null && thread.contains(source.node))) entries.push({ value: entry.value, hadRecentInput: entry.hadRecentInput }) }
  const observer = new PerformanceObserver(list => record(list.getEntries()))
  observer.observe({ type: 'layout-shift' })
  try {
    await userEvent.click(opener)
    const dialog = await canvas.findByRole('dialog', { name: 'Agent navigation' })
    await waitFor(() => expect(dialog.contains(document.activeElement)).toBe(true))
    await waitFor(() => assertModalIsolation(dialog, thread))
    // Inject the non-modal regression into the same rendered dialog, not a second fixture.
    dialog.removeAttribute('aria-modal')
    try { await expect(() => assertModalIsolation(dialog, thread)).toThrow('Navigation dialog must declare aria-modal=true') }
    finally { dialog.setAttribute('aria-modal', 'true') }
    const hiddenBackground = thread.closest<HTMLElement>('[inert], [aria-hidden="true"]')!
    const inert = hiddenBackground.getAttribute('inert'), ariaHidden = hiddenBackground.getAttribute('aria-hidden')
    hiddenBackground.removeAttribute('inert'); hiddenBackground.removeAttribute('aria-hidden')
    try { await expect(() => assertModalIsolation(dialog, thread)).toThrow('Workspace must be hidden or inert while navigation is open') }
    finally {
      if (inert !== null) hiddenBackground.setAttribute('inert', inert)
      if (ariaHidden !== null) hiddenBackground.setAttribute('aria-hidden', ariaHidden)
    }
    assertModalIsolation(dialog, thread)
    await expect(canvas.getByRole('complementary', { name: 'Agents' }).getBoundingClientRect().width).toBeGreaterThanOrEqual(sidebarMinimumWidth)
    await expect(rect()).toEqual(before)
    await userEvent.keyboard('{Shift>}{Tab}{/Shift}')
    await expect(dialog.contains(document.activeElement)).toBe(true)
    await userEvent.keyboard('{Escape}')
    await waitFor(() => expect(root.querySelector('[role="dialog"]')).toBeNull())
    await waitFor(() => expect(document.activeElement).toBe(opener))
    await expect(thread.closest('[inert], [aria-hidden="true"]')).toBeNull()
    await userEvent.keyboard('{Control>}b{/Control}')
    const reopened = await canvas.findByRole('dialog', { name: 'Agent navigation' })
    await waitFor(() => expect(reopened.contains(document.activeElement)).toBe(true))
    await waitFor(() => assertModalIsolation(reopened, thread))
    const search = within(reopened).getByRole('searchbox', { name: 'Search agents' })
    await userEvent.click(search)
    await userEvent.keyboard('{Control>}b{/Control}')
    await expect(root.querySelector('[role="dialog"]')).not.toBeNull()
    await userEvent.click(within(reopened).getByRole('row', { name: /Reviewer session/ }))
    await waitFor(() => expect(root.querySelector('[role="dialog"]')).toBeNull())
    await waitFor(() => expect(document.activeElement).toBe(opener))
    await expect(thread.closest('[inert], [aria-hidden="true"]')).toBeNull()
    await userEvent.keyboard('{Meta>}b{/Meta}')
    const commandOpened = await canvas.findByRole('dialog', { name: 'Agent navigation' })
    await waitFor(() => assertModalIsolation(commandOpened, thread))
    await userEvent.keyboard('{Escape}')
    await waitFor(() => expect(document.activeElement).toBe(opener))
    await expect(root.querySelector('[role="dialog"]')).toBeNull()
    await expect(thread.closest('[inert], [aria-hidden="true"]')).toBeNull()
    await expect(rect()).toEqual(before)
    await new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve())))
    record(observer.takeRecords())
    const threadShift = entries.reduce((total, entry) => total + entry.value, 0)
    await expect(threadShift).toBe(0)
    root.dataset.drawerMeasurement = JSON.stringify({ viewport, presentation: 'drawer', threadBefore: before, threadAfter: rect(), threadRawLayoutShift: threadShift, entries, focusEntered: true, escapeRestoredFocus: true, selectionRestoredFocus: true, shortcutOpened: true, commandShortcutOpened: true, editableShortcutIgnored: true, ariaModal: true, backgroundHidden: true, nonModalControlRejected: true, unmaskedBackgroundControlRejected: true })
  } finally { observer.disconnect() }
}

const meta = { title: 'wf/Sidebar/Responsive navigation', component: ResponsiveSidebarSurface, args: { viewport: 440 }, parameters: { layout: 'fullscreen', docs: { description: { component: 'Navigation has a 240px floor. Below 900px it is a closed-by-default React Aria modal drawer: aria-modal=true and the background workspace is inert or hidden from assistive technology until dismissal. The thread keeps its full width. Escape or selection closes it and restores opener focus. Cmd/Ctrl+B opens navigation outside editable/terminal input. Opening the drawer must produce zero raw thread layout shift, including input-associated entries. Play tests reject injected non-modal and unmasked-background regressions.' } } } } satisfies Meta<typeof ResponsiveSidebarSurface>
export default meta
type Story = StoryObj<typeof meta>
export const At440: Story = { args: { viewport: 440 }, play: drawerPlay }
export const At700: Story = { args: { viewport: 700 }, play: drawerPlay }
export const At900: Story = { args: { viewport: 900 }, play: drawerPlay }
export const At1440: Story = { args: { viewport: 1440 }, play: drawerPlay }
export const At440Light: Story = { args: { viewport: 440, scheme: 'light' }, play: drawerPlay }
export const At1440Light: Story = { args: { viewport: 1440, scheme: 'light' }, play: drawerPlay }
const assertFrozenTrack = (track: HTMLElement): void => {
  if (track.hasAttribute('tabindex') || track.hasAttribute('role')) throw new Error('Frozen resize track must not be focusable or interactive')
  if (getComputedStyle(track).cursor === 'col-resize' || getComputedStyle(track).cursor === 'row-resize') throw new Error('Frozen resize track must not advertise resizing')
}

export const AllStates: Story = {
  render: () => <div {...stylex.props(styles.matrix)}>{(['light', 'dark'] as const).flatMap(scheme => [440, 700, 900, 1440].map(viewport => <section key={`${scheme}-${viewport}`}><h2>{scheme} · {viewport}px</h2><ResponsiveSidebarSurface viewport={viewport} scheme={scheme} shortcut={false} /></section>))}{(['fixed', 'reversed', 'nonfinite'] as const).map(range => <section key={range}><h2>{range} resize range</h2><ResponsiveSidebarSurface viewport={900} range={range} shortcut={false} /></section>)}</div>,
  play: async ({ canvasElement }) => {
    for (const root of canvasElement.querySelectorAll<HTMLElement>('[data-range]:not([data-range="resizable"])')) {
      const track = root.querySelector<HTMLElement>('[data-resizer-track="live-agent-sidebar"]')!
      await expect(root.querySelector('[data-resizer="live-agent-sidebar"]')).toBeNull()
      await expect(track).toHaveAttribute('aria-hidden', 'true')
      await expect(track.hasAttribute('tabindex')).toBe(false)
      await expect(track.getBoundingClientRect().width).toBe(1)
      await expect(getComputedStyle(track).cursor).not.toBe('col-resize')
      await expect(getComputedStyle(track, '::after').content).toBe('none')
      assertFrozenTrack(track)
      try {
        track.tabIndex = 0
        await expect(() => assertFrozenTrack(track)).toThrow('Frozen resize track must not be focusable or interactive')
      } finally { track.removeAttribute('tabindex') }
      const originalCursor = track.style.cursor
      try {
        track.style.cursor = 'col-resize'
        await expect(() => assertFrozenTrack(track)).toThrow('Frozen resize track must not advertise resizing')
      } finally { track.style.cursor = originalCursor }
      assertFrozenTrack(track)
      track.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowRight', bubbles: true }))
      await expect(root.dataset.resizeCommits).toBe('0')
    }
  },
}

const styles = stylex.create({
  root: { display: 'flex', position: 'relative', height: 'calc(100dvh - var(--wf-devbar-space, 0px))', maxWidth: '100%', overflow: 'hidden', backgroundColor: c.canvas, color: c.fg, fontFamily: t.fontSans, fontSize: t.uiSize, lineHeight: t.uiLeading },
  thread: { display: 'flex', flexDirection: 'column', flexGrow: 1, minWidth: 0, minHeight: 0 },
  // Reproduce the production retained conversation layer around the real sidebar frame.
  article: { padding: s.lg, maxWidth: 720, flexGrow: 1, position: 'relative', zIndex: 1 },
  heading: { fontSize: t.uiSize, marginBlock: s.md },
  search: { margin: s.md, padding: s.md, border: `1px solid ${c.border}`, borderRadius: 8 },
  input: { width: '100%', minWidth: 0, borderWidth: 0, outline: 'none', backgroundColor: 'transparent', color: c.fg },
  roster: { display: 'flex', flexDirection: 'column', padding: s.md, flexGrow: 1, minHeight: 0, overflow: 'hidden' },
  button: { backgroundColor: c.raised, color: c.fg, border: `1px solid ${c.borderStrong}`, borderRadius: 6, padding: s.md },
  chromeBoundary: { position: 'absolute', insetInline: 0, bottom: 0, height: 1, zIndex: 1000, backgroundColor: c.canvas },
  matrix: { display: 'grid', gap: s.lg, overflow: 'auto', padding: s.lg },
})
