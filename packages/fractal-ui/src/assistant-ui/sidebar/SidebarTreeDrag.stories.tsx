import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { expect, userEvent, waitFor, within } from 'storybook/test'
import { Button, Dialog, DialogTrigger, Modal, ModalOverlay, useDragAndDrop, type Key } from 'react-aria-components'
import { SidebarTree, type SidebarTreeHandle, type SidebarTreeItem } from './SidebarTree'
import { sidebarTreeStoryVars as sizes } from './sidebar-tree-story-tokens.stylex'
import { geometryVars as g, spaceVars as s, surfaceVars as surface, textVars as ink, typeVars as t } from '../composition-tokens.stylex'

interface Item extends SidebarTreeItem<Item> { readonly folder: boolean; readonly children: readonly Item[] }
const parcel: Item = { id: 'parcel', textValue: 'Parcel review', depth: 1, folder: false, children: [] }
const makeFixture = (deep: boolean): readonly Item[] => [
  { id: 'origin', textValue: 'Origin folder', depth: 0, folder: true, children: [parcel, ...(deep ? [{ id: 'studio', textValue: 'Studio folder', depth: 1, folder: true, children: [{ id: 'drafts', textValue: 'Drafts folder', depth: 2, folder: true, children: Array.from({ length: 80 }, (_, index) => ({ id: `bucket-${index}`, textValue: `Bucket ${index + 1}`, depth: 3, folder: true, children: [] })) }] }] : [])] },
  { id: 'destination', textValue: 'Destination folder', depth: 0, folder: true, children: [] },
]
const expanded = new Set<Key>(['origin', 'studio', 'drafts', 'destination'])
const onExpandedChange = (_keys: Set<Key>) => {}
const renderRow = (item: Item) => <div {...stylex.props(styles.row)}><Button slot="drag" aria-label={`Drag ${item.textValue}`} {...stylex.props(styles.drag)}><svg aria-hidden viewBox="0 0 16 16" width="100%" height="100%" fill="currentColor"><circle cx="8" cy="4" r="1" /><circle cx="8" cy="8" r="1" /><circle cx="8" cy="12" r="1" /></svg></Button><span {...stylex.props(styles.name)}>{item.textValue}</span></div>

function FilingTree({ deep = false, reveal, landmark = true }: { readonly deep?: boolean; readonly reveal?: Key; readonly landmark?: boolean }) {
  const tree = React.useRef<SidebarTreeHandle>(null)
  const [items, setItems] = React.useState(() => makeFixture(deep))
  const [enabled, setEnabled] = React.useState(true)
  const [parent, setParent] = React.useState('origin')
  const index = React.useMemo(() => {
    const result = new Map<Key, Item>()
    const visit = (items: readonly Item[]) => { for (const item of items) { result.set(item.id, item); visit(item.children) } }
    visit(items)
    return result
  }, [items])
  const { dragAndDropHooks } = useDragAndDrop<Item>({
    isDisabled: !enabled,
    getItems: keys => [...keys].map(key => ({ 'text/plain': String(key) })),
    getAllowedDropOperations: () => ['move'],
    acceptedDragTypes: ['text/plain'],
    shouldAcceptItemDrop: target => index.get(target.key)?.folder === true,
    onItemDrop: async event => {
      const source = event.items.find(item => item.kind === 'text' && item.types.has('text/plain'))
      if (source?.kind !== 'text' || await source.getText('text/plain') !== 'parcel') return
      const destination = index.get(event.target.key)
      if (destination === undefined || !destination.folder) return
      const moved = { ...parcel, depth: destination.depth + 1 }
      const move = (nodes: readonly Item[]): readonly Item[] => nodes.filter(item => item.id !== 'parcel').map(item => {
        const children = move(item.children)
        return item.id === destination.id ? { ...item, children: [...children, moved] } : children.some((child, index) => child !== item.children[index]) || children.length !== item.children.length ? { ...item, children } : item
      })
      setItems(previous => move(previous))
      setParent(String(destination.id))
    },
  })
  // Reveal on mount, as the app reveals its selected agent when the sidebar opens.
  React.useLayoutEffect(() => { if (reveal !== undefined) tree.current?.reveal(reveal) }, [reveal])
  const Root = landmark ? 'main' : 'div'
  return <Root data-testid="filing-tree" data-parent-key={parent} {...stylex.props(styles.root)}>
    <div {...stylex.props(styles.toolbar)}><Button onPress={() => setEnabled(value => !value)}>{enabled ? 'Disable filing' : 'Enable filing'}</Button><Button onPress={() => { setItems(makeFixture(deep)); setParent('origin') }}>Reset filing</Button><output aria-label="Filed parent">{parent}</output></div>
    <div {...stylex.props(styles.viewport)}><SidebarTree ref={tree} aria-label="Filing folders" items={items} expandedKeys={expanded} onExpandedChange={onExpandedChange} dragAndDropHooks={dragAndDropHooks} renderRow={renderRow} /></div>
  </Root>
}
/** The narrow app drawer: a RAC Modal Dialog, whose ButtonContext offers only the `close` slot. */
function FilingDrawer() {
  return <main {...stylex.props(styles.shell)}>
    <DialogTrigger>
      <Button>Expand sidebar</Button>
      <ModalOverlay isDismissable {...stylex.props(styles.overlay)}>
        <Modal {...stylex.props(styles.drawer)}>
          <Dialog aria-label="Agent navigation" {...stylex.props(styles.dialog)}><FilingTree deep reveal="bucket-60" landmark={false} /></Dialog>
        </Modal>
      </ModalOverlay>
    </DialogTrigger>
  </main>
}
const meta = { title: 'Fractal UI/Sidebar tree', component: SidebarTree, parameters: { layout: 'fullscreen', docs: { description: { component: 'Host-owned RAC drag hooks and drag-slot content on the production SidebarTree. Pinned visuals are pointer-transparent during any drag session, including external drags with no local dragstart; drop targets and autoscroll belong to the real tree. Local-drag plays disable filing, prove the same move check rejects, restore it, and perform a real RAC move; the external play first proves the pinned copy owns the band outside a drag session.' } } } } satisfies Meta
export default meta
type Story = StoryObj
const frames = async (count = 6) => { for (let index = 0; index < count; index++) await new Promise<void>(resolve => requestAnimationFrame(() => resolve())) }
/** Asynchronous settling on a contended CI host; assertions are unchanged. */
const settled = { timeout: 10_000 }
const row = (root: HTMLElement, key: string) => root.querySelector<HTMLElement>(`[data-sidebar-tree-key="${key}"]`)!
const assertFiled = async (root: HTMLElement, key: string) => { await expect(root.dataset.parentKey).toBe(key) }

async function keyboardFile(root: HTMLElement, enabled: boolean) {
  const handle = within(row(root, 'parcel')).getByRole('button', { name: 'Drag Parcel review' })
  handle.focus()
  await userEvent.keyboard('{Enter}')
  await frames()
  if (enabled) {
    for (let index = 0; index < 12 && document.activeElement?.getAttribute('aria-label') !== 'Drop on Destination folder'; index++) await userEvent.keyboard('{ArrowDown}')
    await expect(document.activeElement?.getAttribute('aria-label')).toBe('Drop on Destination folder')
    await userEvent.keyboard('{Enter}')
    await waitFor(() => expect(root.dataset.parentKey).toBe('destination'), settled)
  } else {
    await userEvent.keyboard('{ArrowDown}{Enter}{Escape}')
    await frames()
  }
}

// Real DragEvent/DataTransfer dispatch exercises RAC's pointer handlers and keyed
// drop delegate. No hook callback or model mutation is invoked by the play.
// Chromium keeps effectAllowed and dropEffect at 'none' on a script-constructed
// DataTransfer, whereas a live drag session lets dragstart write effectAllowed and
// dragenter/dragover write dropEffect. The instance emulates exactly that session
// state, so RAC's own handlers negotiate the operation unmodified.
function sessionTransfer(effectAllowed: DataTransfer['effectAllowed'] = 'uninitialized') {
  const transfer = new DataTransfer()
  let allowed = effectAllowed
  let effect: DataTransfer['dropEffect'] = 'none'
  Object.defineProperties(transfer, {
    effectAllowed: { get: () => allowed, set: (value: DataTransfer['effectAllowed']) => { allowed = value } },
    dropEffect: { get: () => effect, set: (value: DataTransfer['dropEffect']) => { effect = value } },
  })
  return transfer
}
async function startPointer(root: HTMLElement) {
  const source = row(root, 'parcel')
  const bounds = source.getBoundingClientRect()
  const transfer = sessionTransfer()
  source.dispatchEvent(new DragEvent('dragstart', { bubbles: true, cancelable: true, dataTransfer: transfer, clientX: bounds.left + bounds.width / 2, clientY: bounds.top + bounds.height / 2 }))
  await frames()
  return { source, transfer }
}
function over(transfer: DataTransfer, x: number, y: number) {
  const target = document.elementFromPoint(x, y)!
  for (const type of ['dragenter', 'dragover']) target.dispatchEvent(new DragEvent(type, { bubbles: true, cancelable: true, dataTransfer: transfer, clientX: x, clientY: y }))
  return target
}
async function finishPointer(source: HTMLElement, transfer: DataTransfer, target: Element, x: number, y: number) {
  target.dispatchEvent(new DragEvent('drop', { bubbles: true, cancelable: true, dataTransfer: transfer, clientX: x, clientY: y }))
  source.dispatchEvent(new DragEvent('dragend', { bubbles: true, cancelable: true, dataTransfer: transfer, clientX: x, clientY: y }))
  await frames()
}
async function pointerFile(root: HTMLElement) {
  const { source, transfer } = await startPointer(root)
  const bounds = row(root, 'destination').getBoundingClientRect()
  const x = bounds.left + bounds.width / 2
  const y = bounds.top + bounds.height / 2
  const target = over(transfer, x, y)
  await frames()
  await finishPointer(source, transfer, target, x, y)
}

export const KeyboardDragFiling: Story = { render: () => <FilingTree />, play: async ({ canvasElement }) => {
  const root = within(canvasElement).getByTestId('filing-tree')
  await frames()
  await userEvent.click(within(root).getByRole('button', { name: 'Disable filing' }))
  await keyboardFile(root, false)
  await expect(assertFiled(root, 'destination')).rejects.toThrow()
  await userEvent.click(within(root).getByRole('button', { name: 'Enable filing' }))
  await keyboardFile(root, true)
  await assertFiled(root, 'destination')
} }
export const PointerDragFiling: Story = { render: () => <FilingTree />, play: async ({ canvasElement }) => {
  const root = within(canvasElement).getByTestId('filing-tree')
  await frames()
  await userEvent.click(within(root).getByRole('button', { name: 'Disable filing' }))
  await pointerFile(root)
  await expect(assertFiled(root, 'destination')).rejects.toThrow()
  await userEvent.click(within(root).getByRole('button', { name: 'Enable filing' }))
  await pointerFile(root)
  await waitFor(() => assertFiled(root, 'destination'), settled)
} }
export const DragThroughPinnedBand: Story = { render: () => <FilingTree deep />, play: async ({ canvasElement }) => {
  const root = within(canvasElement).getByTestId('filing-tree')
  const tree = within(root).getByRole('treegrid')
  await frames()
  await userEvent.click(within(root).getByRole('button', { name: 'Disable filing' }))
  const rejected = await startPointer(root)
  tree.scrollTop = 600
  await frames()
  let bounds = tree.getBoundingClientRect()
  let x = bounds.left + bounds.width / 2
  let y = bounds.top + 12
  const blocked = over(rejected.transfer, x, y)
  await finishPointer(rejected.source, rejected.transfer, blocked, x, y)
  await expect(assertFiled(root, 'destination')).rejects.toThrow()
  await expect(root.dataset.parentKey).toBe('origin')
  // Control: with no drag session the pinned band owns the scroller's top edge.
  await waitFor(() => expect(document.elementFromPoint(bounds.left + bounds.width / 2, bounds.top + 2)!.closest('[data-pinned-key]')).not.toBeNull(), settled)
  tree.scrollTop = 0
  await frames()
  await userEvent.click(within(root).getByRole('button', { name: 'Enable filing' }))
  const drag = await startPointer(root)
  tree.scrollTop = 600
  await frames()
  const stack = root.querySelector<HTMLElement>('[data-sidebar-pinned-stack]')!
  await expect(stack.querySelectorAll('[data-pinned-key]').length).toBeGreaterThan(0)
  for (const copy of stack.querySelectorAll<HTMLElement>('[data-pinned-key]')) {
    await expect(copy.draggable).toBe(false)
    await expect(getComputedStyle(copy).pointerEvents).toBe('none')
    await expect(copy.querySelector('[inert]')).not.toBeNull()
  }
  // Chromium and Firefox autoscroll natively during a real drag, against the scroller under the
  // pointer (RAC's own autoscroll runs only in macOS WebKit). The top-edge point must therefore
  // hit the treegrid scroller, not the pinned band; the control above proved the band owns it
  // outside a drag session.
  bounds = tree.getBoundingClientRect()
  x = bounds.left + bounds.width / 2
  y = bounds.top + 2
  await expect(document.elementFromPoint(x, y)!.closest('[role="treegrid"]')).toBe(tree)
  over(drag.transfer, x, y)
  await frames()
  await expect(stack.querySelectorAll('[data-pinned-key]').length).toBeGreaterThan(0)
  y = bounds.top + Math.min(12, stack.getBoundingClientRect().height / 2)
  const target = over(drag.transfer, x, y)
  await expect(target.closest('[data-pinned-key]')).toBeNull()
  await expect(target.closest('[role="treegrid"]')).toBe(tree)
  await frames()
  // RAC resolves the keyed drop target from the point; the hit may be a row or the treegrid gap
  // opened for its insertion indicator, never the visual copy.
  const indicator = root.querySelector<HTMLElement>('[data-sidebar-drop-indicator]')!
  await expect(indicator).not.toBeNull()
  const key = indicator.dataset.dropKey!
  await expect(key).toMatch(/^bucket-/)
  await expect(Number(getComputedStyle(indicator).zIndex)).toBeGreaterThan(Number(getComputedStyle(stack).zIndex))
  await finishPointer(drag.source, drag.transfer, target, x, y)
  await waitFor(() => assertFiled(root, key), settled)
} }
// Hit-test one point over the pinned band: passes only when the tree scroller, not a pinned copy, is on top.
const treeAt = async (tree: HTMLElement, x: number, y: number) => {
  const hit = document.elementFromPoint(x, y)!
  await expect(hit.closest('[data-pinned-key]')).toBeNull()
  await expect(hit.closest('[role="treegrid"]')).toBe(tree)
}
// A file-manager style external source offers copy or move and sets no in-page dragstart.
const externalTransfer = () => {
  const transfer = sessionTransfer('copyMove')
  transfer.setData('text/plain', 'parcel')
  return transfer
}
// The browser fires dragenter/dragover at the first hit element, then re-hit-tests on the next dragover.
const externalEnter = (transfer: DataTransfer, x: number, y: number) => {
  const first = document.elementFromPoint(x, y)!
  for (const type of ['dragenter', 'dragover']) first.dispatchEvent(new DragEvent(type, { bubbles: true, cancelable: true, dataTransfer: transfer, clientX: x, clientY: y }))
  return first
}
export const ExternalDragOverPinnedBand: Story = { render: () => <FilingTree deep />, play: async ({ canvasElement }) => {
  const root = within(canvasElement).getByTestId('filing-tree')
  const tree = within(root).getByRole('treegrid')
  await frames()
  tree.scrollTop = 600
  await frames()
  const stack = root.querySelector<HTMLElement>('[data-sidebar-pinned-stack]')!
  await expect(stack.querySelectorAll('[data-pinned-key]').length).toBeGreaterThan(0)
  const bounds = tree.getBoundingClientRect()
  const x = bounds.left + bounds.width / 2
  const y = bounds.top + Math.min(12, stack.getBoundingClientRect().height / 2)
  // Control: with no drag session the pinned copy owns the point, so the tree check rejects.
  await expect(treeAt(tree, x, y)).rejects.toThrow()
  // An external drag (no local dragstart) that leaves the window restores the band.
  const probe = externalTransfer()
  await expect(externalEnter(probe, x, y).closest('[data-pinned-key]')).not.toBeNull()
  await frames()
  await treeAt(tree, x, y)
  const probeTarget = over(probe, x, y)
  await frames()
  await expect(root.querySelector<HTMLElement>('[data-sidebar-drop-indicator]')?.dataset.dropKey).toMatch(/^bucket-/)
  probeTarget.dispatchEvent(new DragEvent('dragleave', { bubbles: true, cancelable: true, dataTransfer: probe, relatedTarget: null, clientX: -1, clientY: -1 }))
  await waitFor(() => expect(treeAt(tree, x, y)).rejects.toThrow(), settled)
  await waitFor(() => expect(root.querySelector('[data-sidebar-drop-indicator]')).toBeNull(), settled)
  await expect(root.dataset.parentKey).toBe('origin')
  // A second external drag enters over the band and drops on the real row underneath.
  const transfer = externalTransfer()
  await expect(externalEnter(transfer, x, y).closest('[data-pinned-key]')).not.toBeNull()
  await frames()
  await treeAt(tree, x, y)
  const target = over(transfer, x, y)
  await frames()
  const indicator = root.querySelector<HTMLElement>('[data-sidebar-drop-indicator]')!
  await expect(indicator).not.toBeNull()
  const key = indicator.dataset.dropKey!
  await expect(key).toMatch(/^bucket-/)
  target.dispatchEvent(new DragEvent('drop', { bubbles: true, cancelable: true, dataTransfer: transfer, clientX: x, clientY: y }))
  await waitFor(() => assertFiled(root, key), settled)
} }
// Pinned and measured copies render host rows outside RAC's TreeItem. Inside the drawer's Dialog they
// must still resolve the row's `drag` slot instead of the Dialog's `close`-only slots.
export const DrawerRevealAt440: Story = { render: () => <FilingDrawer />, play: async ({ canvasElement }) => {
  await userEvent.click(within(canvasElement).getByRole('button', { name: 'Expand sidebar' }))
  const dialog = await within(document.body).findByRole('dialog', { name: 'Agent navigation' }, settled)
  const root = within(dialog).getByTestId('filing-tree')
  await waitFor(() => expect(row(root, 'bucket-60')).not.toBeNull(), settled)
  await expect(within(row(root, 'bucket-60')).getByRole('button', { name: 'Drag Bucket 61' })).toBeVisible()
  const stack = root.querySelector<HTMLElement>('[data-sidebar-pinned-stack]')!
  await waitFor(() => expect(stack.querySelectorAll('[data-pinned-key]').length).toBeGreaterThan(0), settled)
  for (const copy of stack.querySelectorAll<HTMLElement>('[data-pinned-key]')) await expect(copy.querySelector('[inert] button[aria-label^="Drag "]')).not.toBeNull()
} }
const styles = stylex.create({
  root: { width: sizes.small, color: ink.fg, backgroundColor: surface.sidebar, fontFamily: t.fontSans, fontSize: t.metaSize },
  toolbar: { display: 'flex', flexWrap: 'wrap', gap: s.sm, padding: s.sm },
  viewport: { height: sizes.viewport, minHeight: 0 },
  row: { height: sizes.row, display: 'flex', alignItems: 'center', gap: s.sm, minWidth: 0 },
  drag: { width: g.controlSm, height: g.controlSm, padding: 0, flexShrink: 0, color: ink.fgMuted, backgroundColor: 'transparent', borderWidth: 0 },
  name: { overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', minWidth: 0 },
  shell: { width: sizes.small, padding: s.sm, color: ink.fg, backgroundColor: surface.sidebar, fontFamily: t.fontSans, fontSize: t.metaSize },
  overlay: { position: 'fixed', inset: 0, zIndex: 30, backgroundColor: surface.scrim },
  drawer: { position: 'fixed', top: 0, bottom: 0, insetInlineStart: 0, width: sizes.small, maxWidth: '100vw', backgroundColor: surface.sidebar },
  dialog: { height: '100%', outline: 'none' },
})
