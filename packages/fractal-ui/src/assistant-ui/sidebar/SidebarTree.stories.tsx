import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { expect, userEvent, waitFor, within } from 'storybook/test'
import type { Key } from 'react-aria-components'
import { SidebarTree, type SidebarTreeHandle, type SidebarTreeItem } from './SidebarTree'
import { lightTheme } from '../composition-theme'
import { spaceVars as s, surfaceVars as surface, textVars as ink, typeVars as t } from '../composition-tokens.stylex'
import { sidebarTreeStoryVars as sizes } from './sidebar-tree-story-tokens.stylex'

interface Item extends SidebarTreeItem<Item> { readonly children: readonly Item[] }
const makeBranch = (depth: number, prefix: string): Item => ({ id: prefix, textValue: `Folder ${prefix}`, depth, children: depth < 6 ? [makeBranch(depth + 1, `${prefix}.a`), ...Array.from({ length: 12 }, (_, index) => ({ id: `${prefix}.${index}`, textValue: `Review ${index + 1}`, depth: depth + 1, children: [] }))] : Array.from({ length: 70 }, (_, index) => ({ id: `${prefix}.${index}`, textValue: `Review ${index + 1}`, depth: depth + 1, children: [] })) })
const fixture: readonly Item[] = [makeBranch(0, 'north'), makeBranch(0, 'south')]
const deepKey = 'north.a.a.a.a.a.a.35'
const index = new Map<Key, Item>()
const parents = new Map<Key, readonly Key[]>()
function visit(items: readonly Item[], chain: readonly Key[] = []) { for (const item of items) { index.set(item.id, item); parents.set(item.id, chain); visit(item.children, [...chain, item.id]) } }
visit(fixture)
const expanded = new Set([...index.values()].filter(item => item.children.length).map(item => item.id))
function Example({ sticky = true, cold = false, width = 440, light = false, noSettle = false }: { readonly sticky?: boolean; readonly cold?: boolean; readonly width?: number; readonly light?: boolean; readonly noSettle?: boolean }) {
  const [keys, setKeys] = React.useState<Set<Key>>(() => cold ? new Set() : new Set(expanded))
  const [filtered, setFiltered] = React.useState(false)
  const [reverse, setReverse] = React.useState(false)
  const [resized, setResized] = React.useState(false)
  const handle = React.useRef<SidebarTreeHandle>(null)
  const root = React.useRef<HTMLDivElement>(null)
  const filter = (items: readonly Item[]): readonly Item[] => items.filter(item => item.children.length > 0 || Number(String(item.id).split('.').at(-1)) % 2 === 1).map(item => ({ ...item, children: filter(item.children) }))
  const filteredItems = filtered ? filter(fixture) : fixture
  const items = reverse ? [...filteredItems].reverse() : filteredItems
  React.useLayoutEffect(() => {
    if (!cold) return
    handle.current?.reveal(deepKey, noSettle ? { anchor: false } : undefined)
    const frame = requestAnimationFrame(() => setResized(true))
    let probeFrame: number | undefined
    if (!noSettle) {
      const firstExposure = () => {
        const example = root.current
        const target = example === null ? null : real(example, deepKey)
        if (example === null) return
        if (target === null) { probeFrame = requestAnimationFrame(firstExposure); return }
        const bounds = target.getBoundingClientRect()
        const view = viewport(example).getBoundingClientRect()
        if (bounds.bottom <= view.top || bounds.top >= view.bottom) { probeFrame = requestAnimationFrame(firstExposure); return }
        const inset = Math.min(view.height * 0.4, bounds.height * 4)
        example.dataset.firstRevealClear = String(bounds.top >= view.top + inset - 1 && bounds.bottom <= view.bottom + 1)
      }
      probeFrame = requestAnimationFrame(firstExposure)
    }
    return () => { cancelAnimationFrame(frame); if (probeFrame !== undefined) cancelAnimationFrame(probeFrame) }
  }, [cold, noSettle])
  return <section data-example={sticky && !noSettle ? 'sticky' : 'control'} ref={root} {...stylex.props(styles.example, light && lightTheme, width === 240 ? styles.narrow : width === 1440 ? styles.wide : styles.small)}>
    <div {...stylex.props(styles.toolbar)}><button onClick={() => handle.current?.reveal(deepKey)}>Reveal review</button><button onClick={() => handle.current?.reveal('south.a.a.a.a.a.a.35')}>Reveal unvisited branch</button><button onClick={() => handle.current?.reveal('north.a.a.a.a.a.a.69')}>Reveal boundary</button><button onClick={() => setFiltered(value => !value)}>Filter reviews</button><button onClick={() => setKeys(new Set())}>Collapse all</button><button onClick={() => setReverse(value => !value)}>Reorder</button></div>
    <div {...stylex.props(styles.viewport)}><SidebarTree ref={handle} aria-label="Project reviews" items={items} expandedKeys={keys} onExpandedChange={setKeys} stickyAncestors={sticky} renderRow={item => <div {...stylex.props(styles.label, resized && styles.resized)}><span {...stylex.props(styles.labelText)}>{item.textValue}</span></div>} /></div>
  </section>
}
// The cold control keeps stickiness and disables measured settling instead: with 24px estimates a
// non-sticky single scroll already lands clear, so only the sticky band can expose an unsettled reveal.
function Pair({ cold = false, width = 440, light = false }: { readonly cold?: boolean; readonly width?: number; readonly light?: boolean }) { return <main {...stylex.props(styles.matrix)}><Example cold={cold} width={width} light={light} /><Example sticky={cold} noSettle={cold} cold={cold} width={width} light={light} /></main> }
const meta = { title: 'Fractal UI/Sidebar tree', component: SidebarTree, parameters: { layout: 'fullscreen', docs: { description: { component: 'A measured RAC treegrid. Sticky copies are hidden from accessibility and inert; every regression includes a production-component comparison with stickiness disabled, or for the cold reveal with measured settling disabled. RAC represents tree items as treegrid rows.' } } } } satisfies Meta
export default meta
type Story = StoryObj
const frames = async () => { await document.fonts.ready; for (let i = 0; i < 6; i++) await new Promise<void>(resolve => requestAnimationFrame(() => resolve())) }
/** Asynchronous settling (reveal retries, ResizeObserver delivery) on a contended CI host; assertions are unchanged. */
const settled = { timeout: 10_000 }
const examples = (canvas: HTMLElement) => [canvas.querySelector('[data-example="sticky"]')!, canvas.querySelector('[data-example="control"]')!] as const
const viewport = (example: Element) => example.querySelector('[role="treegrid"]') as HTMLElement
const stack = (example: Element) => example.querySelector('[data-sidebar-pinned-stack]') as HTMLElement
const real = (example: Element, key: string) => example.querySelector(`[data-sidebar-tree-key="${key}"]`) as HTMLElement | null
const clear = (example: Element, row: HTMLElement) => { const bounds = row.getBoundingClientRect(); const view = viewport(example).getBoundingClientRect(); return bounds.top >= view.top + stack(example).getBoundingClientRect().height - 1 && bounds.bottom <= view.bottom + 1 }
async function scroll(example: Element, offset = 600) { viewport(example).dispatchEvent(new WheelEvent('wheel', { bubbles: true })); viewport(example).scrollTop = offset; await frames() }
async function assertChain(example: Element) {
  const top = stack(example).dataset.topKey!
  const actual = [...example.querySelectorAll('[data-pinned-ancestors]')].flatMap(row => row.getAttribute('data-pinned-ancestors')!.split(','))
  await expect(actual).toEqual(parents.get(top))
}
const pair = () => <Pair />
export const StickyChain: Story = { render: pair, play: async ({ canvasElement }) => { const [yes, no] = examples(canvasElement); await frames(); await scroll(yes); await scroll(no); await assertChain(yes); await expect(no.querySelectorAll('[data-pinned-key]').length).toBe(0); await expect(parents.get(stack(no).dataset.topKey!)!.length).toBeGreaterThan(0) } }
export const PushOutBoundary: Story = { render: pair, play: async ({ canvasElement }) => {
  const [yes, no] = examples(canvasElement); await frames()
  await userEvent.click(within(yes as HTMLElement).getByRole('button', { name: 'Reveal boundary' }))
  await waitFor(() => expect(real(yes, 'north.a.a.a.a.a.a.69')).not.toBeNull(), settled)
  await frames()
  const last = real(yes, 'north.a.a.a.a.a.a.69')!
  const boundary = viewport(yes).scrollTop + last.getBoundingClientRect().bottom - viewport(yes).getBoundingClientRect().top
  let moved = false
  // Intersect the moving painted/hit layer with its transparent clipping slot.
  for (const distance of [90, 72, 54, 36, 18]) {
    await scroll(yes, boundary - distance)
    const visible = [...yes.querySelectorAll('[data-pinned-key]')].map(pin => {
      const layer = pin.getBoundingClientRect()
      const slot = pin.parentElement!.getBoundingClientRect()
      if (layer.top < slot.top - 1) moved = true
      return { top: Math.max(layer.top, slot.top), bottom: Math.min(layer.bottom, slot.bottom) }
    }).filter(rect => rect.bottom > rect.top)
    for (let i = 1; i < visible.length; i++) await expect(visible[i]!.top).toBeGreaterThanOrEqual(visible[i - 1]!.bottom - 1)
  }
  await expect(moved).toBe(true)
  await scroll(no); await expect(no.querySelectorAll('[data-pinned-key]').length).toBe(0)
} }
export const DepthCap: Story = { render: pair, play: async ({ canvasElement }) => { const [yes, no] = examples(canvasElement); await frames(); await scroll(yes); await scroll(no); await expect(yes.querySelector('[data-sidebar-breadcrumb]')).not.toBeNull(); await expect(stack(yes).getBoundingClientRect().height).toBeLessThanOrEqual(viewport(yes).clientHeight * 0.4); await expect(yes.querySelectorAll('[data-pinned-key]').length).toBeLessThanOrEqual(4); await expect(no.querySelector('[data-sidebar-breadcrumb]')).toBeNull() } }
export const AccessibleRows: Story = { render: pair, play: async ({ canvasElement }) => { const [yes, no] = examples(canvasElement); await frames(); await scroll(yes); await scroll(no); for (const example of [yes, no]) { await expect(within(example as HTMLElement).getAllByRole('treegrid')).toHaveLength(1); await expect(within(example as HTMLElement).getAllByRole('row')).toHaveLength(example.querySelectorAll('[data-sidebar-tree-key]').length); await expect(stack(example).querySelectorAll('[role="row"]')).toHaveLength(0); await expect(stack(example).getAttribute('aria-hidden')).toBe('true') } await expect(yes.querySelectorAll('[data-pinned-key]').length).toBeGreaterThan(0); await expect(no.querySelectorAll('[data-pinned-key]').length).toBe(0) } }
export const PinnedClick: Story = { render: pair, play: async ({ canvasElement }) => { const [yes, no] = examples(canvasElement); await frames(); await scroll(yes); await scroll(no); const pin = [...yes.querySelectorAll('[data-pinned-key]')].at(-1) as HTMLElement; const key = pin.dataset.pinnedKey!; await userEvent.click(pin); await waitFor(() => expect(document.activeElement).toBe(real(yes, key)), settled); await frames(); await expect(clear(yes, real(yes, key)!)).toBe(true); await expect(no.querySelector('[data-pinned-key]')).toBeNull() } }
export const FocusAndReveal: Story = { render: pair, play: async ({ canvasElement }) => { const [yes, no] = examples(canvasElement); await frames(); await scroll(yes); await scroll(no); const top = stack(yes).dataset.topKey!; real(yes, top)!.focus(); await frames(); await expect(clear(yes, real(yes, top)!)).toBe(true); await userEvent.keyboard('{ArrowDown}'); await frames(); await expect(clear(yes, document.activeElement as HTMLElement)).toBe(true); await userEvent.click(within(yes as HTMLElement).getByRole('button', { name: 'Reveal review' })); await waitFor(() => expect(real(yes, deepKey)).not.toBeNull(), settled); await frames(); await expect(clear(yes, real(yes, deepKey)!)).toBe(true); await expect(no.querySelector('[data-pinned-key]')).toBeNull() } }
export const VisibleFocusDoesNotJump: Story = { render: pair, play: async ({ canvasElement }) => {
  const [yes, no] = examples(canvasElement)
  await frames()
  await scroll(yes)
  await scroll(no)
  for (const example of [yes, no]) {
    const view = viewport(example)
    const bounds = view.getBoundingClientRect()
    const inset = stack(example).getBoundingClientRect().height
    const row = [...view.querySelectorAll<HTMLElement>('[data-sidebar-tree-key]')].find(row => {
      const rect = row.getBoundingClientRect()
      return rect.top > bounds.top + inset + rect.height && rect.bottom < bounds.bottom - rect.height
    })!
    await expect(row).toBeDefined()
    const before = view.scrollTop
    // The disabled-stickiness control deliberately reproduces the previous exact
    // focus alignment, so this assertion has a genuinely failing comparison.
    let controlFrame: number | undefined
    const naiveFocus = () => {
      controlFrame = requestAnimationFrame(() => {
        view.dispatchEvent(new WheelEvent('wheel', { bubbles: true }))
        view.scrollTop += row.getBoundingClientRect().top - view.getBoundingClientRect().top
      })
    }
    if (example === no) view.addEventListener('focusin', naiveFocus)
    try {
      row.focus()
      await frames()
      if (example === yes) await expect(view.scrollTop).toBe(before)
      else await expect(Math.abs(view.scrollTop - before)).toBeGreaterThan(1)
    } finally {
      view.removeEventListener('focusin', naiveFocus)
      if (controlFrame !== undefined) cancelAnimationFrame(controlFrame)
    }
  }
} }
export const CollapseFilterReorder: Story = { render: pair, play: async ({ canvasElement }) => {
  const [yes, no] = examples(canvasElement)
  await frames()
  for (const example of [yes, no]) {
    await scroll(example)
    await userEvent.click(within(example as HTMLElement).getByRole('button', { name: 'Filter reviews' }))
    await frames()
    if (example === yes) await assertChain(yes)
    await userEvent.click(within(example as HTMLElement).getByRole('button', { name: 'Reorder' }))
    await frames()
    const row = viewport(example).querySelector<HTMLElement>('[data-sidebar-tree-key]')!
    row.focus()
    await userEvent.keyboard('{Home}')
    await frames()
    await expect((document.activeElement as HTMLElement).getAttribute('data-sidebar-tree-key')).toBe('south')
    await expect(stack(example).dataset.topKey).toBe('south')
    await scroll(example)
    await expect(stack(example).dataset.topKey).toMatch(/^south/)
    if (example === yes) await assertChain(yes)
    else await expect(example.querySelectorAll('[data-pinned-key]')).toHaveLength(0)
    await userEvent.click(within(example as HTMLElement).getByRole('button', { name: 'Collapse all' }))
    await frames()
    await expect(example.querySelectorAll('[data-pinned-key]')).toHaveLength(0)
    await expect(within(example as HTMLElement).getAllByRole('row')).toHaveLength(2)
  }
} }
export const ColdRevealBeforeMeasurement: Story = { render: () => <Pair cold />, play: async ({ canvasElement }) => {
  const yes = canvasElement.querySelector('[data-example="sticky"]')!
  const no = canvasElement.querySelector('[data-example="control"]')!
  await waitFor(() => expect(real(yes, deepKey)).not.toBeNull(), settled)
  await waitFor(() => expect((yes as HTMLElement).dataset.firstRevealClear).toBe('true'), settled)
  await frames()
  await expect(real(yes, deepKey)!.getBoundingClientRect().height).toBe(32)
  await expect(clear(yes, real(yes, deepKey)!)).toBe(true)
  await frames()
  await expect(clear(yes, real(yes, deepKey)!)).toBe(true)
  // The unsettled control is compared once its ancestors are measured and pinned.
  await waitFor(() => expect(no.querySelectorAll('[data-pinned-key]').length).toBeGreaterThan(0), settled)
  await frames()
  const control = real(no, deepKey)
  await expect(control).not.toBeNull()
  await expect(clear(no, control!)).toBe(false)
} }
export const RevealUnvisitedAncestors: Story = { render: pair, play: async ({ canvasElement }) => {
  const [yes, no] = examples(canvasElement)
  await frames()
  for (const example of [yes, no]) {
    await userEvent.click(within(example as HTMLElement).getByRole('button', { name: 'Reveal unvisited branch' }))
    await waitFor(() => expect(real(example, 'south.a.a.a.a.a.a.35')).not.toBeNull(), settled)
    await frames()
  }
  await expect(clear(yes, real(yes, 'south.a.a.a.a.a.a.35')!)).toBe(true)
  await assertChain(yes)
  const pins = [...yes.querySelectorAll('[data-pinned-key]')]
  await expect(pins.slice(-3).map(pin => pin.getAttribute('data-pinned-key'))).toEqual(['south.a.a.a.a', 'south.a.a.a.a.a', 'south.a.a.a.a.a.a'])
  for (const pin of pins) await expect(pin.getBoundingClientRect().height).toBe(24)
  await expect(stack(yes).getBoundingClientRect().height).toBeLessThanOrEqual(viewport(yes).clientHeight * 0.4)
  await expect(no.querySelectorAll('[data-pinned-key]')).toHaveLength(0)
} }
export const Light440: Story = { render: () => <Pair light /> }
export const Dark1440: Story = { render: () => <Pair width={1440} /> }
export const Light1440: Story = { render: () => <Pair width={1440} light /> }
/**
 * At 240px the breadcrumb truncates inside its own block box, the only kind of box that paints
 * text-overflow, and the padded crumb itself never overflows, so no glyph is cut at its edge.
 */
async function assertBreadcrumbEllipsis(canvas: HTMLElement) {
  const [yes] = examples(canvas)
  await frames()
  await scroll(yes)
  const crumb = yes.querySelector<HTMLElement>('[data-sidebar-breadcrumb]')
  await expect(crumb).not.toBeNull()
  const text = crumb!.querySelector<HTMLElement>('[data-sidebar-breadcrumb-text]')
  await expect(text).not.toBeNull()
  const style = getComputedStyle(text!)
  await expect({ display: style.display, overflowX: style.overflowX, whiteSpace: style.whiteSpace, textOverflow: style.textOverflow }).toEqual({ display: 'block', overflowX: 'hidden', whiteSpace: 'nowrap', textOverflow: 'ellipsis' })
  await expect(text!.scrollWidth).toBeGreaterThan(text!.clientWidth)
  await expect(crumb!.scrollWidth).toBeLessThanOrEqual(crumb!.clientWidth)
  const contentEnd = crumb!.getBoundingClientRect().right - parseFloat(getComputedStyle(crumb!).paddingRight)
  await expect(text!.getBoundingClientRect().right).toBeLessThanOrEqual(contentEnd + 0.5)
}
export const Modal240: Story = { render: () => <Pair width={240} />, play: ({ canvasElement }) => assertBreadcrumbEllipsis(canvasElement) }
export const Modal240Light: Story = { render: () => <Pair width={240} light />, play: ({ canvasElement }) => assertBreadcrumbEllipsis(canvasElement) }
export const AllStates: Story = { render: () => <main {...stylex.props(styles.matrix)}>{[240, 440, 1440].flatMap(width => [false, true].map(light => <Example key={`${width}-${light}`} width={width} light={light} />))}</main> }
const styles = stylex.create({
  matrix: { display: 'flex', flexWrap: 'wrap', alignItems: 'flex-start', gap: s.xl, padding: s.xl, backgroundColor: surface.canvas },
  example: { minWidth: 0, color: ink.fg, backgroundColor: surface.sidebar, fontFamily: t.fontSans, fontSize: t.metaSize },
  narrow: { width: sizes.narrow }, small: { width: sizes.small }, wide: { width: sizes.wide },
  toolbar: { display: 'flex', flexWrap: 'wrap', gap: s.xs, padding: s.xs },
  viewport: { height: sizes.viewport, minHeight: 0 },
  label: { display: 'flex', alignItems: 'center', height: sizes.row, overflow: 'hidden', whiteSpace: 'nowrap', paddingInline: s.sm, minWidth: 0 },
  labelText: { minWidth: 0, overflow: 'hidden', whiteSpace: 'nowrap', textOverflow: 'ellipsis' },
  resized: { height: sizes.resizedRow },
})
