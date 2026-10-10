import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { createPortal } from 'react-dom'
import { displayedTabKey, paneKey, paneKind, type WorkbenchLayout, type WorkbenchPane, type TabGroupLayout } from './workbench-model'
import { readStoredActiveTab } from './workbench-state'

interface PaneMount {
  readonly id: number
  readonly pane: WorkbenchPane
  readonly element: HTMLDivElement
  readonly close: () => void
  group: TabGroupLayout
  onClose?: () => void
  restoreFocus?: HTMLElement
}
interface PaneSlots {
  readonly byPath: Map<string, PaneMount>
}
interface PanePlacement {
  readonly mount: PaneMount
  readonly group: TabGroupLayout
}
const PaneSlotsContext = React.createContext<PaneSlots | undefined>(undefined)
export const PaneCloseContext = React.createContext<(() => void) | undefined>(undefined)

/** Resource objects survive immutable layout edits. A mount has its own identity, not a layout path.
 * Separate occurrences of the same object still receive separate mounts; matching groups win before
 * the remaining occurrence is reused when adding a tab replaces its group object. */
export function usePaneLayer(layout: WorkbenchLayout, workspaceId: string, focusedKey: string | undefined) {
  const [owner] = React.useState(() => ({ nextId: 0, mounts: new Map<WorkbenchPane, PaneMount[]>(), selected: new WeakMap<TabGroupLayout, string>() }))
  const groups: { node: TabGroupLayout; path: string; activeKey: string }[] = []
  const visit = (node: WorkbenchLayout, path: string) => {
    if (node.kind === 'group') {
      const activeKey = displayedTabKey(node.tabs.map(paneKey), focusedKey, owner.selected.get(node) ?? readStoredActiveTab(workspaceId, path))
      groups.push({ node, path, activeKey })
    } else { visit(node.children[0], `${path}.0`); visit(node.children[1], `${path}.1`) }
  }
  visit(layout, '0')
  const assigned = new Map<PaneMount, TabGroupLayout>()
  const pending: { pane: WorkbenchPane; node: TabGroupLayout; path: string; activeKey: string }[] = []
  const visible: PanePlacement[] = []
  const byPath = new Map<string, PaneMount>()
  const activate = (mount: PaneMount, node: TabGroupLayout, path: string, activeKey: string) => {
    assigned.set(mount, node)
    if (paneKey(mount.pane) === activeKey) {
      byPath.set(path, mount)
      visible.push({ mount, group: node })
    }
  }
  // Reserve exact surviving groups first, so a changed group cannot steal a duplicate's identity.
  for (const { node, path, activeKey } of groups) for (const pane of node.tabs) {
    const mount = owner.mounts.get(pane)?.find(candidate => candidate.group === node && !assigned.has(candidate))
    if (mount) activate(mount, node, path, activeKey)
    else pending.push({ pane, node, path, activeKey })
  }
  for (const { pane, node, path, activeKey } of pending) {
    let pool = owner.mounts.get(pane)
    if (!pool) { pool = []; owner.mounts.set(pane, pool) }
    let mount = pool.find(candidate => !assigned.has(candidate))
    if (!mount) {
      const element = document.createElement('div')
      element.className = stylex.props(styles.host).className ?? ''
      const id = owner.nextId++
      element.dataset.paneInstance = String(id)
      const created: PaneMount = { id, pane, element, group: node, close: () => created.onClose?.() }
      pool.push(created)
      mount = created
    }
    activate(mount, node, path, activeKey)
  }
  React.useLayoutEffect(() => {
    // Only a committed layout may retire a host or update group selection.
    for (const [mount, node] of assigned) mount.group = node
    for (const { node, activeKey } of groups) owner.selected.set(node, activeKey)
    for (const [pane, pool] of owner.mounts) {
      const retained = pool.filter(mount => assigned.has(mount))
      if (retained.length === 0) owner.mounts.delete(pane)
      else if (retained.length !== pool.length) owner.mounts.set(pane, retained)
    }
  })
  const slots = React.useMemo(() => ({ byPath }), [layout, workspaceId, focusedKey])
  return { slots, visible }
}

/** Layout owns only empty slots. React owns each pane once, under this stable portal layer. */
export function PaneLayer({ visible, canClose, render }: { visible: readonly PanePlacement[]; canClose: (group: TabGroupLayout) => boolean; render: (pane: WorkbenchPane) => React.ReactNode }) {
  return visible.map(({ mount, group }) => createPortal(
    <PaneCloseContext.Provider value={paneKind(mount.pane) === 'diff' && canClose(group) ? mount.close : undefined}>{render(mount.pane)}</PaneCloseContext.Provider>,
    mount.element,
    String(mount.id),
  ))
}

export const PaneSlotsProvider = PaneSlotsContext.Provider

export function usePaneSlotKey(path: string): string {
  const mount = React.useContext(PaneSlotsContext)?.byPath.get(path)
  if (!mount) throw new Error(`Missing active pane at ${path}`)
  return paneKey(mount.pane)
}

export function PaneSlot({ pane, path, onClose }: { pane: WorkbenchPane; path: string; onClose?: () => void }) {
  const slots = React.useContext(PaneSlotsContext)
  const mount = slots?.byPath.get(path)
  if (!mount || mount.pane !== pane) throw new Error(`Missing pane mount at ${path}`)
  React.useLayoutEffect(() => { mount.onClose = onClose }, [mount, onClose])
  const ref = React.useRef<HTMLDivElement>(null)
  React.useLayoutEffect(() => {
    const slot = ref.current!
    slot.appendChild(mount.element)
    mount.restoreFocus?.focus({ preventScroll: true })
    mount.restoreFocus = undefined
    return () => {
      const active = mount.element.ownerDocument.activeElement
      if (active instanceof HTMLElement && mount.element.contains(active)) mount.restoreFocus = active
      // A promoted sibling may already have attached the same host to its new slot.
      if (mount.element.parentElement === slot) slot.removeChild(mount.element)
    }
  }, [mount])
  return <div ref={ref} data-pane-slot={paneKey(pane)} {...stylex.props(styles.host)} />
}

const styles = stylex.create({
  host: { display: 'flex', flexDirection: 'column', flexGrow: 1, minWidth: 0, minHeight: 0 },
})
