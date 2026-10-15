import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { useLocale } from 'react-aria'
import { Button, ButtonContext, CheckboxContext, CheckboxFieldContext, Collection, DEFAULT_SLOT, DragAndDropContext, ListLayout, Size, Tree, TreeItem, TreeItemContent, Virtualizer, type Key, type ListLayoutOptions, type TreeItemRenderProps, type TreeProps } from 'react-aria-components'
import { accentVars as accent, borderVars as border, geometryVars as g, spaceVars as s, surfaceVars as surface, textVars as ink } from '../composition-tokens.stylex'
import { scrollToLayoutKey } from '../composition/virtual-layout'
import { Icon } from '../composition/Icons'

export interface SidebarTreeItem<T> {
  readonly id: Key
  readonly depth: number
  readonly children?: readonly T[]
  /** RAC typeahead text, and the row's accessible name unless `accessibleName` is given. */
  readonly textValue: string
  /**
   * The row's accessible name, when it should say more than `textValue`. Start it with the
   * visible title so the name contains the label. RAC names the row from these item props,
   * so the name holds while memoized row content skips rendering after a reposition.
   */
  readonly accessibleName?: string
}
export interface SidebarTreeHandle { reveal(key: Key, options?: { readonly anchor: false }): void }
export interface SidebarTreeProps<T extends SidebarTreeItem<T>> {
  readonly items: readonly T[]
  readonly renderRow: (item: T, state: { depth: number; expanded: boolean; pinned: boolean }) => React.ReactNode
  readonly expandedKeys: ReadonlySet<Key>
  readonly onExpandedChange: (keys: Set<Key>) => void
  readonly onAction?: (key: Key) => void
  readonly dragAndDropHooks?: TreeProps<T>['dragAndDropHooks']
  readonly 'aria-label': string
  readonly ref?: React.Ref<SidebarTreeHandle>
  readonly style?: stylex.StyleXStyles
  readonly onRowContextMenu?: (item: T, event: React.MouseEvent) => void
  /** Comparison control; normal trees leave this enabled. */
  readonly stickyAncestors?: boolean
}
type Pin<T> = { item: T; height: number; offset: number; breadcrumb?: readonly T[] }
type ItemDrop = { key: Key; top: number; height: number }
type Snapshot<T> = { pins: readonly Pin<T>[]; height: number; width: number; top?: Key; measurements: readonly T[]; drop?: ItemDrop }
type DropTarget = NonNullable<React.ContextType<typeof DragAndDropContext>['dropState']>['target']
/** Browsers repeat dragover roughly every 50ms while a drag hovers the window. */
const dragLeaveGraceMs = 150
/**
 * Rows are never shorter than their `g.controlSm` (24px) min-height. Estimating unmeasured rows
 * at that minimum lets RAC mount every row that can be visible in one layout pass, so measuring
 * can only shrink the mounted range. A larger estimate mounts a few rows, measures them, and
 * re-renders until the viewport fills, and filtering drops RAC's layout for hidden rows, so
 * that cascade would repeat on every keystroke.
 */
const layoutOptions = { estimatedRowHeight: 24 } as const

interface SidebarTreeLayoutOptions extends ListLayoutOptions {
  /** The host's `renderRow`. A new identity means row content, and so any measured height, may have changed. */
  readonly rowContent: unknown
}
interface MeasuredRow { readonly width: number; readonly height: number; readonly rowContent: unknown }

/**
 * Keeps RAC's own virtualized insertion indicators above the visual ancestor band, and keeps
 * each row's measured height across filtering. RAC drops the layout of every row a filter hides
 * and re-estimates every row whose collection node changed, then re-measures each mounted one
 * with a forced layout. A retained item object at the same width and `renderRow` instead reuses
 * the height last measured for it.
 */
class SidebarTreeLayout extends ListLayout<unknown, SidebarTreeLayoutOptions> {
  private rowContent: unknown
  private readonly measured = new WeakMap<object, MeasuredRow>()
  override getDropTargetLayoutInfo(target: Parameters<ListLayout<unknown>['getDropTargetLayoutInfo']>[0]) {
    const info = super.getDropTargetLayoutInfo(target)
    info.zIndex = 2
    return info
  }
  override shouldInvalidateLayoutOptions(next: SidebarTreeLayoutOptions, previous: SidebarTreeLayoutOptions) {
    return super.shouldInvalidateLayoutOptions(next, previous) || next.rowContent !== previous.rowContent
  }
  override update(invalidationContext: Parameters<ListLayout<unknown, SidebarTreeLayoutOptions>['update']>[0]) {
    this.rowContent = invalidationContext.layoutOptions?.rowContent
    super.update(invalidationContext)
  }
  protected override buildItem(...args: Parameters<ListLayout<unknown>['buildItem']>) {
    const built = super.buildItem(...args)
    const value = args[0].value
    const known = typeof value === 'object' && value !== null ? this.measured.get(value) : undefined
    const info = built.layoutInfo
    if (info.estimatedSize && known !== undefined && known.rowContent === this.rowContent && known.width === info.rect.width) {
      info.rect.height = known.height
      info.estimatedSize = false
      built.validRect = info.rect.intersection(this.requestedRect)
    }
    return built
  }
  override updateItemSize(key: Key, size: Size) {
    const changed = super.updateItemSize(key, size)
    const value = this.virtualizer?.collection.getItem(key)?.value
    const info = this.layoutNodes.get(key)?.layoutInfo
    if (typeof value === 'object' && value !== null && info !== undefined) this.measured.set(value, { width: info.rect.width, height: size.height, rowContent: this.rowContent })
    return changed
  }
}

/** Public RAC context supplies keyboard and pointer sessions without wrapping host hooks. */
function DragSessionObserver({ onChange }: { readonly onChange: (dragging: boolean, target: DropTarget) => void }) {
  const { dragState, dropState } = React.useContext(DragAndDropContext)
  const dragging = (dragState?.draggingKeys.size ?? 0) > 0
  const target = dropState?.target ?? null
  React.useLayoutEffect(() => onChange(dragging, target), [dragging, target, onChange])
  return null
}

// RAC's TreeItem gives row content these slots. Pinned and measured copies render host rows outside
// any TreeItem, so they get the same slot names without the real row's handlers. Otherwise a host
// `slot="drag"` would resolve against an ancestor's slots and throw, such as a Dialog's `close`.
const copyButtonSlots: React.ContextType<typeof ButtonContext> = { slots: { [DEFAULT_SLOT]: {}, chevron: {}, drag: {} } }
const copyCheckboxSlots: React.ContextType<typeof CheckboxContext> = { slots: { [DEFAULT_SLOT]: {}, selection: {} } }
const copyCheckboxFieldSlots: React.ContextType<typeof CheckboxFieldContext> = { slots: { [DEFAULT_SLOT]: {}, selection: {} } }

interface RowContentProps<T extends SidebarTreeItem<T>> {
  readonly item: T
  readonly expanded: boolean
  readonly pinned: boolean
  readonly renderRow: SidebarTreeProps<T>['renderRow']
}
/**
 * Host content re-renders only when its inputs change. RAC re-renders a row's TreeItem when the
 * virtualizer repositions it or the tree re-renders; a retained item at a new offset reuses its content.
 */
const RowContent = React.memo(function RowContent<T extends SidebarTreeItem<T>>({ item, expanded, pinned, renderRow }: RowContentProps<T>) {
  const content = renderRow(item, { depth: item.depth, expanded, pinned })
  return pinned
    ? <ButtonContext.Provider value={copyButtonSlots}><CheckboxContext.Provider value={copyCheckboxSlots}><CheckboxFieldContext.Provider value={copyCheckboxFieldSlots}>{content}</CheckboxFieldContext.Provider></CheckboxContext.Provider></ButtonContext.Provider>
    : content
}) as <T extends SidebarTreeItem<T>>(props: RowContentProps<T>) => React.ReactNode

/** RAC owns the sole accessible tree; sticky rows are pointer-only visual proxies. */
export function SidebarTree<T extends SidebarTreeItem<T>>(props: SidebarTreeProps<T>) {
  const { direction } = useLocale()
  const layout = React.useMemo(() => new SidebarTreeLayout(layoutOptions), [])
  const rowLayoutOptions = React.useMemo(() => ({ rowContent: props.renderRow }), [props.renderRow])
  const tree = React.useRef<HTMLDivElement>(null)
  const navigation = React.useRef<TreeItemRenderProps['state'] | undefined>(undefined)
  const frame = React.useRef<number | undefined>(undefined)
  const pending = React.useRef<Key | undefined>(undefined)
  const anchorMode = React.useRef<'reveal' | 'focus' | 'once'>('focus')
  const stickyInset = React.useRef(0)
  const awaitingExpansion = React.useRef(false)
  const settlingAttempts = React.useRef(0)
  const measurementBank = React.useRef<HTMLDivElement>(null)
  const nativeDrag = React.useRef(false)
  const dropTarget = React.useRef<DropTarget>(null)
  const [dragging, setDragging] = React.useState(false)
  const [snapshot, setSnapshot] = React.useState<Snapshot<T>>({ pins: [], height: 0, width: 0, measurements: [] })
  const model = React.useMemo(() => {
    const all = new Map<Key, T>()
    const domKeys = new Map<string, Key>()
    const parents = new Map<Key, readonly T[]>()
    const visible: T[] = []
    const walk = (items: readonly T[], ancestors: readonly T[], shown: boolean) => {
      for (const item of items) {
        all.set(item.id, item)
        domKeys.set(String(item.id), item.id)
        parents.set(item.id, ancestors)
        if (shown) visible.push(item)
        walk(item.children ?? [], [...ancestors, item], shown && props.expandedKeys.has(item.id))
      }
    }
    walk(props.items, [], true)
    return { all, domKeys, parents, visible }
  }, [props.items, props.expandedKeys])
  // Reanchor before paint after RAC commits measured row positions. This path reads
  // only RAC geometry, never DOM geometry, including during observer delivery.
  const applyMeasuredAnchor = React.useEffectEvent(() => {
    const key = pending.current
    const viewport = tree.current
    const virtualizer = layout.virtualizer
    if (key === undefined || viewport === null || virtualizer === null || virtualizer.visibleRect.height <= 0) return false
    const ancestors = model.parents.get(key)
    if (ancestors === undefined || ancestors.some(item => !props.expandedKeys.has(item.id))) return false
    const info = layout.getLayoutInfo(key)
    if (info == null) return false
    if (anchorMode.current === 'once') {
      scrollToLayoutKey(layout, key, viewport)
      pending.current = undefined
      return true
    }
    const rect = virtualizer.visibleRect
    let offset = rect.y
    if (anchorMode.current === 'reveal') {
      const infos = ancestors.map(item => layout.getLayoutInfo(item.id))
      if (infos.some(info => info == null || info.estimatedSize)) return false
      const inset = props.stickyAncestors === false ? 0 : Math.min(rect.height * 0.4, infos.reduce((sum, info) => sum + info!.rect.height, 0))
      offset = Math.max(0, info.rect.y - inset)
    } else {
      const inset = props.stickyAncestors === false ? 0 : stickyInset.current
      if (info.rect.y < rect.y + inset) offset = Math.max(0, info.rect.y - inset)
      else if (info.rect.maxY > rect.maxY) offset = info.rect.maxY - rect.height
    }
    if (Math.abs(rect.y - offset) > 0.5) scrollToLayoutKey(layout, key, viewport, info.rect.y - offset)
    return true
  })

  // All viewport reads are batched here. Geometry for rows comes only from RAC layout infos.
  const measure = React.useEffectEvent(() => {
    const viewport = tree.current
    if (viewport === null || layout.virtualizer === null) return
    const scroll = viewport.scrollTop
    const height = viewport.clientHeight
    const width = viewport.clientWidth
    const rows = model.visible.flatMap(item => {
      const info = layout.getLayoutInfo(item.id)
      return info == null ? [] : [{ item, rect: info.rect, estimated: info.estimatedSize }]
    })
    const top = rows.find(row => row.rect.maxY > scroll)
    const ancestors = top === undefined ? [] : model.parents.get(top.item.id) ?? []
    const measurementCandidates = new Map<Key, T>()
    for (const item of [...ancestors, ...(pending.current === undefined ? [] : model.parents.get(pending.current) ?? [])]) {
      if (layout.getLayoutInfo(item.id)?.estimatedSize) measurementCandidates.set(item.id, item)
    }
    let candidates = ancestors.flatMap(item => {
      const row = rows.find(row => row.item.id === item.id)
      return row === undefined || row.estimated || row.rect.y >= scroll ? [] : [{ item, height: row.rect.height, offset: 0 }]
    })
    if (props.stickyAncestors === false) candidates = []
    const budget = height * 0.4
    let pins: Pin<T>[] = candidates
    // Retain deepest rows. The breadcrumb uses the real upper row's measured height.
    if (pins.length > 4 || pins.reduce((sum, pin) => sum + pin.height, 0) > budget) {
      let keep = Math.min(3, pins.length - 1)
      while (keep > 0 && pins.slice(-keep).reduce((sum, pin) => sum + pin.height, pins[0]!.height) > budget) keep--
      const compressed = pins.slice(0, pins.length - keep)
      pins = [{ ...compressed[compressed.length - 1]!, height: Math.min(pins[0]!.height, budget), breadcrumb: compressed.map(pin => pin.item) }, ...pins.slice(pins.length - keep)]
    }
    let slot = 0
    pins = pins.map((pin, index) => {
      // The first row outside this ancestor's subtree pushes the entire remaining stack.
      const boundary = rows.find(row => row.rect.y > scroll && row.item.depth <= pin.item.depth && !(model.parents.get(row.item.id) ?? []).some(parent => parent.id === pin.item.id))
      const remaining = pins.slice(index).reduce((sum, row) => sum + row.height, 0)
      const offset = Math.min(0, (boundary?.rect.y ?? Infinity) - scroll - slot - remaining)
      const result = { ...pin, offset }
      slot += pin.height
      return result
    })
    const measurements = [...measurementCandidates.values()]
    const target = dropTarget.current
    const targetInfo = target?.type === 'item' && target.dropPosition === 'on' ? layout.getLayoutInfo(target.key) : undefined
    const drop = target?.type === 'item' && targetInfo != null ? { key: target.key, top: targetInfo.rect.y - scroll, height: targetInfo.rect.height } : undefined
    const next = { pins, height: pins.reduce((sum, pin) => sum + pin.height, 0), width, top: top?.item.id, measurements, drop }
    stickyInset.current = next.height
    setSnapshot(previous => previous.top === next.top && previous.height === next.height && previous.width === width && previous.drop?.key === drop?.key && previous.drop?.top === drop?.top && previous.drop?.height === drop?.height && previous.measurements.length === measurements.length && previous.measurements.every((item, index) => item === measurements[index]) && previous.pins.length === pins.length && previous.pins.every((pin, index) => pin.item === pins[index]!.item && pin.height === pins[index]!.height && pin.offset === pins[index]!.offset && pin.breadcrumb?.map(item => item.id).join(',') === pins[index]!.breadcrumb?.map(item => item.id).join(',')) ? previous : next)
    const key = pending.current
    if (key !== undefined && model.all.has(key)) {
      // Retry while ancestors/data/observed sizes settle, including late ResizeObserver deliveries.
      const ancestors = model.parents.get(key) ?? []
      if (ancestors.some(item => !props.expandedKeys.has(item.id))) {
        if (!awaitingExpansion.current) { pending.current = undefined; return }
        const expanded = new Set(props.expandedKeys)
        for (const ancestor of ancestors) expanded.add(ancestor.id)
        props.onExpandedChange(expanded)
        return
      }
      awaitingExpansion.current = false
      const info = layout.getLayoutInfo(key)
      if (info != null) {
        const inset = props.stickyAncestors === false ? 0 : Math.min(budget, ancestors.reduce((sum, parent) => { const info = layout.getLayoutInfo(parent.id); return sum + (info?.estimatedSize === false ? info.rect.height : 0) }, 0))
        settlingAttempts.current++
        // The first scroll already includes every measured ancestor. Do not expose
        // a provisional no-inset target while waiting for unrelated row estimates.
        if (applyMeasuredAnchor()) return
        if (settlingAttempts.current < 8) { schedule(); return }
        if (info.rect.y < scroll + inset || info.rect.maxY > scroll + height) scrollToLayoutKey(layout, key, viewport, inset)
      }
    }
  })
  const schedule = React.useCallback(() => {
    if (frame.current !== undefined) return
    frame.current = requestAnimationFrame(() => { frame.current = undefined; measure() })
  }, [])
  const observeDrag = React.useEffectEvent((active: boolean, target: DropTarget) => {
    dropTarget.current = target
    if (active || target !== null) pending.current = undefined
    setDragging(nativeDrag.current || active || target !== null)
    schedule()
  })
  const dragObserver = React.useCallback((active: boolean, target: DropTarget) => observeDrag(active, target), [])
  // Any native drag session over the document makes the pinned band transparent, including
  // drags from another window or the OS that never fire a local dragstart and may enter
  // directly over the band. The browser re-hit-tests every dragover, so the next one reaches
  // the real row underneath.
  React.useLayoutEffect(() => {
    if (props.dragAndDropHooks === undefined) return
    let leave: number | undefined
    const cancelLeave = () => { if (leave !== undefined) { window.clearTimeout(leave); leave = undefined } }
    const active = () => {
      cancelLeave()
      if (nativeDrag.current) return
      nativeDrag.current = true
      pending.current = undefined
      setDragging(true)
    }
    const end = () => { cancelLeave(); nativeDrag.current = false; dropTarget.current = null; setDragging(false); schedule() }
    // A null relatedTarget marks leaving the window; some engines also report it between
    // elements, so the session only ends when no dragenter/dragover follows within a tick
    // of the browser's continuous dragover cadence.
    const leaveWindow = (event: DragEvent) => {
      if (event.relatedTarget !== null || !nativeDrag.current) return
      cancelLeave()
      leave = window.setTimeout(end, dragLeaveGraceMs)
    }
    for (const type of ['dragstart', 'dragenter', 'dragover'] as const) document.addEventListener(type, active, true)
    for (const type of ['dragend', 'drop'] as const) document.addEventListener(type, end, true)
    document.addEventListener('dragleave', leaveWindow, true)
    return () => {
      cancelLeave()
      for (const type of ['dragstart', 'dragenter', 'dragover'] as const) document.removeEventListener(type, active, true)
      for (const type of ['dragend', 'drop'] as const) document.removeEventListener(type, end, true)
      document.removeEventListener('dragleave', leaveWindow, true)
    }
  }, [props.dragAndDropHooks, schedule])
  const reveal = React.useEffectEvent((key: Key, options?: { readonly anchor: false }) => {
    pending.current = key
    anchorMode.current = options?.anchor === false ? 'once' : 'reveal'
    awaitingExpansion.current = true
    settlingAttempts.current = 0
    const expanded = new Set(props.expandedKeys)
    for (const ancestor of model.parents.get(key) ?? []) expanded.add(ancestor.id)
    props.onExpandedChange(expanded)
    schedule()
  })
  React.useImperativeHandle(props.ref, () => ({ reveal }), [])
  // Mounted rows report their natural size here, after layout, so RAC never re-measures a
  // mounted row by resetting its height inside a forced layout. A hidden tree reports zero
  // heights; the last measurement stands until it is shown again.
  const resizeRows = React.useEffectEvent((entries: readonly ResizeObserverEntry[]) => {
    for (const entry of entries) {
      const id = (entry.target as HTMLElement).dataset.sidebarTreeKey
      const key = id === undefined ? undefined : model.domKeys.get(id)
      const box = entry.borderBoxSize[0]
      if (key !== undefined && box !== undefined && box.blockSize > 0) layout.virtualizer?.updateItemSize(key, new Size(box.inlineSize, box.blockSize))
    }
    schedule()
  })
  React.useLayoutEffect(() => {
    schedule()
    const viewport = tree.current
    if (viewport === null) return
    const observer = new ResizeObserver(entries => resizeRows(entries))
    observer.observe(viewport)
    const rows = new Set<Element>()
    const track = () => {
      for (const row of rows) if (!row.isConnected) { observer.unobserve(row); rows.delete(row) }
      for (const row of viewport.querySelectorAll('[data-sidebar-tree-key]')) if (!rows.has(row)) { observer.observe(row); rows.add(row) }
    }
    const mutations = new MutationObserver(records => {
      applyMeasuredAnchor()
      track()
      // RAC rebinds a reused row element to another key without remounting it. Observing it
      // again resets its last reported size, so the rebound row reports its size once.
      for (const record of records) if (record.attributeName === 'data-sidebar-tree-key' && record.target instanceof Element && record.target.isConnected) observer.observe(record.target)
      schedule()
    })
    mutations.observe(viewport, { childList: true, attributes: true, attributeFilter: ['style', 'data-sidebar-tree-key'], subtree: true })
    track()
    return () => { observer.disconnect(); mutations.disconnect(); if (frame.current !== undefined) cancelAnimationFrame(frame.current); frame.current = undefined }
  }, [schedule])
  React.useLayoutEffect(() => {
    const bank = measurementBank.current
    if (bank === null) return
    const observer = new ResizeObserver(entries => {
      for (const entry of entries) {
        const id = (entry.target as HTMLElement).dataset.measureKey
        const item = snapshot.measurements.find(item => String(item.id) === id)
        if (item === undefined || layout.virtualizer === null) continue
        const size = layout.getContentSize().copy()
        size.width = entry.contentRect.width
        size.height = entry.contentRect.height
        layout.virtualizer.updateItemSize(item.id, size)
      }
      applyMeasuredAnchor()
      schedule()
    })
    for (const row of bank.children) observer.observe(row)
    return () => observer.disconnect()
  }, [snapshot.measurements, layout, schedule, props.renderRow])
  React.useLayoutEffect(schedule, [model, schedule])
  const focusReal = (key: Key) => {
    reveal(key)
    navigation.current?.selectionManager.setFocused(true)
    navigation.current?.selectionManager.setFocusedKey(key)
    props.onAction?.(key)
  }
  const keyboard = (event: React.KeyboardEvent<HTMLDivElement>) => {
    const state = navigation.current
    if (state === undefined || !(event.target instanceof HTMLElement) || event.target.getAttribute('role') !== 'row' || event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return
    const expand = direction === 'rtl' ? 'ArrowLeft' : 'ArrowRight'
    const collapse = direction === 'rtl' ? 'ArrowRight' : 'ArrowLeft'
    if (event.key !== expand && event.key !== collapse) return
    const key = state.selectionManager.focusedKey
    const node = key === null ? undefined : state.collection.getItem(key)
    if (node == null) return
    const child = [...state.collection.getChildren?.(node.key) ?? []].find(node => node.type === 'item')
    let destination: Key | undefined
    if (event.key === expand) {
      if (child === undefined) return
      if (state.expandedKeys.has(node.key)) destination = child.key
      else state.toggleKey(node.key)
    } else if (child !== undefined && state.expandedKeys.has(node.key)) state.toggleKey(node.key)
    else {
      const parent = node.parentKey == null ? undefined : state.collection.getItem(node.parentKey)
      if (parent?.type !== 'item') return
      destination = parent.key
    }
    event.preventDefault()
    event.stopPropagation()
    if (destination !== undefined) {
      state.selectionManager.setFocused(true)
      state.selectionManager.setFocusedKey(destination)
    }
  }
  const row = (item: T, expanded: boolean, pinned: boolean) => <div {...stylex.props(styles.row)} style={{ paddingInlineStart: `calc(${s.lg} * ${item.depth})` }}>
    {Array.from({ length: item.depth }, (_, depth) => <span key={depth} aria-hidden {...stylex.props(styles.guide)} style={{ insetInlineStart: `calc(${s.lg} * ${depth} + ${s.sm})` }} />)}
    {item.children?.length ? pinned ? <span data-pinned-chevron {...stylex.props(styles.chevron, direction === 'rtl' && styles.mirrored)}><Icon name={expanded ? 'chevron-down' : 'chevron-right'} size={12} /></span> : <Button slot="chevron" {...stylex.props(styles.chevron, direction === 'rtl' && styles.mirrored)}><Icon name={expanded ? 'chevron-down' : 'chevron-right'} size={12} /></Button> : null}
    <div {...stylex.props(styles.content)}><RowContent item={item} expanded={expanded} pinned={pinned} renderRow={props.renderRow} /></div>
    {!pinned && props.dragAndDropHooks !== undefined && <DragSessionObserver onChange={dragObserver} />}
  </div>
  const renderItem = (item: T): React.ReactElement => <TreeItem id={item.id} textValue={item.textValue} aria-label={item.accessibleName ?? item.textValue} data-sidebar-tree-key={String(item.id)} onContextMenu={event => props.onRowContextMenu?.(item, event)} className={({ isFocusVisible }) => stylex.props(styles.item, isFocusVisible && styles.focused).className ?? ''}>
    <TreeItemContent>{({ isExpanded }) => row(item, isExpanded, false)}</TreeItemContent>
    <Collection items={item.children ?? []} dependencies={[props.renderRow, props.onRowContextMenu, props.dragAndDropHooks !== undefined]}>{renderItem}</Collection>
  </TreeItem>
  return <div {...stylex.props(styles.root, props.style)} data-sidebar-drag-active={dragging || undefined} onKeyDownCapture={event => { pending.current = undefined; keyboard(event) }} onScrollCapture={schedule} onWheelCapture={() => { pending.current = undefined }} onPointerDownCapture={() => { pending.current = undefined }} onFocusCapture={event => {
    const target = (event.target as HTMLElement).closest('[data-sidebar-tree-key]')
    const key = target?.getAttribute('data-sidebar-tree-key')
    const item = model.visible.find(item => String(item.id) === key)
    if (item !== undefined) {
      if (pending.current !== item.id) anchorMode.current = 'focus'
      pending.current = item.id
      awaitingExpansion.current = false
      schedule()
    }
  }}>
    <Virtualizer layout={layout} layoutOptions={rowLayoutOptions}><Tree ref={tree} aria-label={props['aria-label']} items={props.items} dependencies={[props.renderRow, props.onRowContextMenu, props.dragAndDropHooks !== undefined]} selectionMode="none" expandedKeys={props.expandedKeys} onExpandedChange={props.onExpandedChange} onAction={props.onAction} dragAndDropHooks={props.dragAndDropHooks} className={({ state }) => { navigation.current = state; return stylex.props(styles.tree).className ?? '' }}>{renderItem}</Tree></Virtualizer>
    <div aria-hidden="true" data-sidebar-pinned-stack data-top-key={snapshot.top} {...stylex.props(styles.stack)} style={{ height: snapshot.height }}>
      {snapshot.pins.map(pin => <div key={pin.item.id} {...stylex.props(styles.pinSlot)} style={{ height: pin.height }}><div draggable={false} data-pinned-key={String(pin.item.id)} data-pinned-ancestors={(pin.breadcrumb ?? [pin.item]).map(item => item.id).join(',')} {...stylex.props(styles.pin, dragging && styles.dragPassthrough)} style={{ height: pin.height, transform: `translateY(calc(${pin.offset} * ${g.hairline}))` }} onClick={event => {
        const target = event.target as HTMLElement
        if (target.closest('[data-pinned-toggle]')) { const keys = new Set(props.expandedKeys); keys.has(pin.item.id) ? keys.delete(pin.item.id) : keys.add(pin.item.id); props.onExpandedChange(keys) }
        else focusReal(pin.item.id)
      }} onContextMenu={event => props.onRowContextMenu?.(pin.item, event)}>
        <div inert {...stylex.props(styles.copy)}>{pin.breadcrumb ? <div data-sidebar-breadcrumb {...stylex.props(styles.breadcrumb)}><span data-sidebar-breadcrumb-text {...stylex.props(styles.breadcrumbText)}>{pin.breadcrumb.map(item => item.textValue).join(' / ')}</span></div> : row(pin.item, props.expandedKeys.has(pin.item.id), true)}</div>
        {pin.item.children?.length && !pin.breadcrumb ? <span data-pinned-toggle {...stylex.props(styles.toggleProxy, dragging && styles.dragPassthrough)} style={{ insetInlineStart: `calc(${s.lg} * ${pin.item.depth})` }} /> : null}
      </div></div>)}
    </div>
    {snapshot.drop !== undefined && <div aria-hidden data-sidebar-drop-indicator data-drop-key={String(snapshot.drop.key)} {...stylex.props(styles.dropIndicator)} style={{ top: snapshot.drop.top, height: snapshot.drop.height, width: snapshot.width }} />}
    <div ref={measurementBank} aria-hidden inert {...stylex.props(styles.measurementBank)} style={{ width: snapshot.width }}>{snapshot.measurements.map(item => <div key={item.id} data-measure-key={String(item.id)}>{row(item, props.expandedKeys.has(item.id), true)}</div>)}</div>
  </div>
}
const styles = stylex.create({
  root: { position: 'relative', height: '100%', minHeight: 0, minWidth: 0, overflow: 'hidden', color: ink.fg },
  tree: { height: '100%', minHeight: 0, overflowY: 'auto', outline: 'none' },
  item: { outline: 'none' },
  focused: { outlineStyle: 'solid', outlineWidth: g.focusRing, outlineOffset: `calc(-1 * ${g.focusRing})`, outlineColor: ink.fgMuted },
  row: { position: 'relative', display: 'flex', alignItems: 'center', minHeight: g.controlSm, minWidth: 0, boxSizing: 'border-box' },
  guide: { position: 'absolute', top: 0, bottom: 0, width: g.hairline, backgroundColor: border.borderStrong, pointerEvents: 'none' },
  chevron: { display: 'flex', alignItems: 'center', justifyContent: 'center', width: g.controlSm, height: g.controlSm, flexShrink: 0, padding: 0, borderWidth: 0, backgroundColor: 'transparent', color: ink.fgMuted },
  // The SVG chevron does not bidi-mirror like the former glyph did; mirror it so collapsed rows point toward the inline end.
  mirrored: { transform: 'scaleX(-1)' },
  content: { flex: 1, minWidth: 0 },
  stack: { position: 'absolute', top: 0, insetInline: 0, zIndex: 1, pointerEvents: 'none', overflow: 'hidden', maxHeight: '40%' },
  pinSlot: { position: 'relative', overflow: 'hidden', pointerEvents: 'none' },
  measurementBank: { position: 'absolute', top: 0, insetInlineStart: 0, visibility: 'hidden', pointerEvents: 'none' },
  pin: { position: 'relative', overflow: 'hidden', backgroundColor: { default: surface.sidebar, ':hover': surface.rowHover }, pointerEvents: 'auto', cursor: 'pointer' },
  dragPassthrough: { pointerEvents: 'none' },
  dropIndicator: { position: 'absolute', insetInlineStart: 0, boxSizing: 'border-box', zIndex: 3, pointerEvents: 'none', borderStyle: 'solid', borderWidth: g.focusRing, borderColor: accent.primary },
  copy: { height: '100%', pointerEvents: 'none' },
  toggleProxy: { position: 'absolute', top: 0, width: g.controlSm, height: '100%' },
  breadcrumb: { height: '100%', display: 'flex', alignItems: 'center', paddingInline: s.md, overflow: 'hidden', fontSize: 'inherit', color: ink.fgMuted },
  // text-overflow paints only in a block container; a flex container's anonymous text is cut mid-glyph.
  breadcrumbText: { minWidth: 0, overflow: 'hidden', whiteSpace: 'nowrap', textOverflow: 'ellipsis' },
})
