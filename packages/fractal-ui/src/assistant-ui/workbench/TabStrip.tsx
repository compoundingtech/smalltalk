import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { TabList, Tab, TooltipTrigger, Tooltip, Focusable } from 'react-aria-components'
import { useFocusVisible } from 'react-aria'
import { Icon, type IconName } from '../composition/Icons'
import { surfaceVars as sf, textVars as tx, accentVars as ac, typeVars as t, radiusVars as r, spaceVars as s, geometryVars as g, geometryNumbers as geo } from '../composition-tokens.stylex'
import { paneKey, paneKind, type WorkbenchPane } from './workbench-model'
import { layoutTabStrip, layoutTabStripViewport, type TabStripMode } from './tab-strip-layout'

export interface TabStripItem {
  readonly key: string
  readonly label: string
  readonly icon: IconName
  readonly kind: string
}

/** Resource identity determines the glyph; two resources of one kind keep the same glyph. */
export function paneTabIdentity(pane: WorkbenchPane): { icon: IconName; kind: string } {
  if (pane.view === 'details') return { icon: 'panel', kind: 'details' }
  const kind = paneKind(pane)
  if (kind === 'thread') return { icon: 'message', kind: 'thread' }
  if (kind === 'diff') return { icon: 'pull-request', kind: 'diff' }
  const scheme = pane.uri.split(':', 1)[0]
  switch (scheme) {
    case 'terminal': case 'pty': return { icon: 'drawer', kind: 'terminal' }
    case 'folder': case 'directory': return { icon: 'folder', kind: 'directory' }
    case 'http': case 'https': case 'link': return { icon: 'attach', kind: 'link' }
    default: return { icon: 'copy', kind: 'document' }
  }
}

export function paneTabItem(pane: WorkbenchPane, label: string): TabStripItem {
  return { key: paneKey(pane), label, ...paneTabIdentity(pane) }
}

/** Fractional padding box: clientWidth/clientHeight round, so only subtract a real scrollbar gutter. */
function paddingBox(element: HTMLElement, rect: DOMRect, css: CSSStyleDeclaration) {
  const right = parseFloat(css.borderRightWidth) || 0, bottom = parseFloat(css.borderBottomWidth) || 0
  const gutterX = element.offsetWidth - element.clientLeft - element.clientWidth - Math.round(right)
  const gutterY = element.offsetHeight - element.clientTop - element.clientHeight - Math.round(bottom)
  return { left: rect.left + element.clientLeft, right: rect.right - right - (gutterX > 1 ? gutterX : 0), top: rect.top + element.clientTop, bottom: rect.bottom - bottom - (gutterY > 1 ? gutterY : 0) }
}

/** Effective viewport includes every clipping ancestor, not merely the retained tab box. */
function stripClip(list: HTMLElement) {
  const clip = paddingBox(list, list.getBoundingClientRect(), getComputedStyle(list))
  for (let ancestor = list.parentElement; ancestor; ancestor = ancestor.parentElement) {
    const css = getComputedStyle(ancestor)
    const clipsX = /^(auto|scroll|hidden|clip)$/.test(css.overflowX), clipsY = /^(auto|scroll|hidden|clip)$/.test(css.overflowY)
    if (!clipsX && !clipsY) continue
    const edge = paddingBox(ancestor, ancestor.getBoundingClientRect(), css)
    if (clipsX) { clip.left = Math.max(clip.left, edge.left); clip.right = Math.min(clip.right, edge.right) }
    if (clipsY) { clip.top = Math.max(clip.top, edge.top); clip.bottom = Math.min(clip.bottom, edge.bottom) }
  }
  return clip
}

/** Mounted below the host's RAC Tabs: selection and resource panels stay host-owned. */
export function TabStrip({ items, selectedKey, label = 'Open panes' }: { items: readonly TabStripItem[]; selectedKey: string; label?: string }) {
  const refreshRef = React.useRef<((revealKey?: string) => void) | null>(null)
  const selectedRef = React.useRef(selectedKey)
  selectedRef.current = selectedKey
  const attach = React.useCallback((root: HTMLDivElement | null) => {
    if (!root) return
    let list = root.querySelector<HTMLElement>('[data-tab-strip]')
    const iconMetric = root.querySelector<HTMLElement>('[data-icon-cell-metric]')!
    const maxMetric = root.querySelector<HTMLElement>('[data-max-cell-metric]')!
    let previousMode: TabStripMode | undefined
    let disposed = false
    let refreshing = false
    const refresh = (revealKey?: string) => {
      if (disposed || refreshing || !list) return
      refreshing = true
      try {
        const tabs = [...list.querySelectorAll<HTMLElement>('[role="tab"]')]
        const metrics = { iconCellWidth: iconMetric.getBoundingClientRect().width, maxCellWidth: maxMetric.getBoundingClientRect().width, gap: parseFloat(getComputedStyle(list).columnGap) || 0, labelPadding: parseFloat(getComputedStyle(tabs[0] ?? list).paddingLeft) || 0 }
        const availableWidth = list.getBoundingClientRect().width
        const result = layoutTabStrip({ availableWidth, items: tabs.map(tab => ({ key: tab.dataset.tabKey!, minimumLabelWidth: tab.querySelector<HTMLElement>('[data-tab-minimum]')!.getBoundingClientRect().width })), metrics, previousMode })
        previousMode = result.mode
        list.dataset.tabMode = result.mode
        list.dataset.tabMeasuredWidth = String(availableWidth)
        list.dataset.tabCollapseBelow = String(result.collapseBelow)
        list.dataset.tabExpandAt = String(result.expandAt)
        root.style.setProperty('--tab-cell-width', `${result.cellWidth}px`)
        for (const tab of tabs) {
          const compact = result.mode === 'icons'
          tab.toggleAttribute('data-icon-only', compact)
          tab.querySelector('[data-tab-icon]')!.toggleAttribute('data-icon-only', compact)
          tab.querySelector('[data-tab-label]')!.toggleAttribute('data-icon-only', compact)
        }
        const cells = tabs.map(tab => { const rect = tab.getBoundingClientRect(); return { key: tab.dataset.tabKey!, left: rect.left, right: rect.right, top: rect.top, bottom: rect.bottom } })
        const viewport = layoutTabStripViewport({ cells, clip: stripClip(list), scrollOffset: list.scrollLeft, revealKey })
        let paintedKeys = viewport.paintedKeys
        if (viewport.scrollOffset !== list.scrollLeft) {
          list.scrollLeft = viewport.scrollOffset
          // Native scroll snapping/clamping owns the final offset. Score the resulting boxes.
          paintedKeys = layoutTabStripViewport({ cells: tabs.map(tab => { const rect = tab.getBoundingClientRect(); return { key: tab.dataset.tabKey!, left: rect.left, right: rect.right, top: rect.top, bottom: rect.bottom } }), clip: stripClip(list), scrollOffset: list.scrollLeft }).paintedKeys
          // A minimum edge adjustment can snap back to the previous cell. If that
          // leaves the requested resource occluded, reveal at its own snap anchor.
          if (revealKey && !paintedKeys.includes(revealKey)) {
            const target = tabs.find(tab => tab.dataset.tabKey === revealKey)
            if (target) {
              list.scrollLeft += target.getBoundingClientRect().left - stripClip(list).left
              paintedKeys = layoutTabStripViewport({ cells: tabs.map(tab => { const rect = tab.getBoundingClientRect(); return { key: tab.dataset.tabKey!, left: rect.left, right: rect.right, top: rect.top, bottom: rect.bottom } }), clip: stripClip(list), scrollOffset: list.scrollLeft }).paintedKeys
            }
          }
        }
        const painted = new Set(paintedKeys)
        for (const tab of tabs) tab.toggleAttribute('data-tab-clipped', !painted.has(tab.dataset.tabKey!))
      } finally { refreshing = false }
    }
    refreshRef.current = refresh
    const revealSelected = () => refresh(selectedRef.current)
    const resize = new ResizeObserver(revealSelected)
    const scroll = () => refresh()
    const ancestorResize = new ResizeObserver(scroll)
    let clippingAncestors: HTMLElement[] = []
    const observe = () => {
      // RAC mounts its DOM TabList after the virtual collection pass.
      const nextList = root.querySelector<HTMLElement>('[data-tab-strip]')
      if (nextList !== list) {
        list?.removeEventListener('scroll', scroll)
        list = nextList
        list?.addEventListener('scroll', scroll, { passive: true })
      }
      resize.disconnect()
      ancestorResize.disconnect()
      for (const ancestor of clippingAncestors) ancestor.removeEventListener('scroll', scroll)
      clippingAncestors = []
      // Ancestor-only resize/scroll changes the paint viewport without changing
      // either the strip's dimensions or its own scroll offset.
      for (let ancestor = list?.parentElement; ancestor; ancestor = ancestor.parentElement) {
        const css = getComputedStyle(ancestor)
        if (!/^(auto|scroll|hidden|clip)$/.test(css.overflowX) && !/^(auto|scroll|hidden|clip)$/.test(css.overflowY)) continue
        clippingAncestors.push(ancestor)
        ancestorResize.observe(ancestor)
        ancestor.addEventListener('scroll', scroll, { passive: true })
      }
      resize.observe(root)
      if (list) resize.observe(list)
      resize.observe(iconMetric)
      resize.observe(maxMetric)
      if (list) for (const probe of list.querySelectorAll('[data-tab-minimum]')) resize.observe(probe)
      revealSelected()
    }
    const mutations = new MutationObserver(observe)
    mutations.observe(root, { childList: true, characterData: true, subtree: true })
    list?.addEventListener('scroll', scroll, { passive: true })
    observe()
    document.fonts?.ready.then(revealSelected)
    document.fonts?.addEventListener('loadingdone', revealSelected)
    return () => {
      disposed = true
      resize.disconnect()
      ancestorResize.disconnect()
      for (const ancestor of clippingAncestors) ancestor.removeEventListener('scroll', scroll)
      mutations.disconnect()
      list?.removeEventListener('scroll', scroll)
      document.fonts?.removeEventListener('loadingdone', revealSelected)
      if (refreshRef.current === refresh) refreshRef.current = null
    }
  }, [])
  React.useLayoutEffect(() => { refreshRef.current?.(selectedKey) }, [items, selectedKey])
  return <div ref={attach} onFocusCapture={event => { const tab = (event.target as HTMLElement).closest<HTMLElement>('[role="tab"]'); if (tab) refreshRef.current?.(tab.dataset.tabKey) }} {...stylex.props(styles.root)}>
    <span aria-hidden="true" data-icon-cell-metric {...stylex.props(styles.metric, styles.iconMetric)} />
    <span aria-hidden="true" data-max-cell-metric {...stylex.props(styles.metric, styles.maxMetric)} />
    <TabList data-tab-strip aria-label={label} {...stylex.props(styles.list)}>
      {items.map(item => <ResourceTab key={item.key} item={item} selected={item.key === selectedKey} />)}
    </TabList>
  </div>
}

function ResourceTab({ item, selected }: { item: TabStripItem; selected: boolean }) {
  const triggerRef = React.useRef<HTMLDivElement>(null)
  const handlersRef = React.useRef<React.HTMLAttributes<Element> | null>(null)
  const focusFrame = React.useRef<number | undefined>(undefined)
  React.useEffect(() => () => { if (focusFrame.current !== undefined) cancelAnimationFrame(focusFrame.current) }, [])
  // RAC's interaction modality: its keyboard reveal and focus tooltips follow it, including synthetic key events.
  const { isFocusVisible } = useFocusVisible()
  const keyboardFocus = React.useRef(isFocusVisible)
  React.useLayoutEffect(() => { keyboardFocus.current = isFocusVisible }, [isFocusVisible])
  const characters = Array.from(item.label)
  const minimum = characters.length > 4 ? `${characters.slice(0, 4).join('')}…` : item.label
  const triggerHandlers = React.useMemo<React.HTMLAttributes<Element>>(() => ({
    onFocus: event => {
      // Reveal/snap happens before focus disclosure. RAC closes overlays on native
      // scroll events, so every reveal scroll must be dispatched before the tooltip
      // opens. The strip's own snap scrolls pre-paint. RAC reveals a keyboard-focused
      // item in the window (scrollIntoViewport) from a later frame callback; when that
      // scrolls the document or an ancestor, its event can arrive after disclosure and
      // close it. Perform the same native reveal now (RAC skips it on a scroll-locked
      // root, so do we): its event precedes this frame callback and RAC's reveal is a no-op.
      const tab = event.currentTarget
      if (keyboardFocus.current && getComputedStyle(document.scrollingElement ?? document.documentElement).overflow !== 'hidden') tab.scrollIntoView({ block: 'nearest', inline: 'nearest' })
      if (focusFrame.current !== undefined) cancelAnimationFrame(focusFrame.current)
      // React clears currentTarget after dispatch; RAC's focus filter needs it.
      const focusEvent = { ...event, currentTarget: tab }
      focusFrame.current = requestAnimationFrame(() => {
        focusFrame.current = undefined
        if (triggerRef.current === document.activeElement) handlersRef.current?.onFocus?.(focusEvent)
      })
    },
    onBlur: event => {
      if (focusFrame.current !== undefined) cancelAnimationFrame(focusFrame.current)
      focusFrame.current = undefined
      handlersRef.current?.onBlur?.(event)
    },
    onPointerEnter: event => handlersRef.current?.onPointerEnter?.(event),
    onPointerLeave: event => handlersRef.current?.onPointerLeave?.(event),
    onMouseEnter: event => handlersRef.current?.onMouseEnter?.(event),
    onMouseLeave: event => handlersRef.current?.onMouseLeave?.(event),
    onPointerDown: event => handlersRef.current?.onPointerDown?.(event),
  }), [])
  const seedTrigger = React.useCallback((element: HTMLSpanElement | null) => { if (element) triggerRef.current = element.closest<HTMLDivElement>('[role="tab"]') }, [])
  return <Tab ref={triggerRef} id={item.key} aria-label={item.label} data-tab-key={item.key} data-tab-kind={item.kind} data-icon-only data-tab-clipped {...triggerHandlers} {...stylex.props(styles.tab, selected && styles.selected)}>
    <span ref={seedTrigger} data-tab-label data-icon-only aria-hidden="true" {...stylex.props(styles.label)}>{item.label}</span>
    <span data-tab-icon data-icon-only aria-hidden="true" {...stylex.props(styles.icon)}><Icon name={item.icon} /></span>
    <span data-tab-minimum aria-hidden="true" {...stylex.props(styles.metric, styles.minimum)}>{minimum}</span>
    <TooltipTrigger delay={350} closeDelay={0}><Focusable><TabTooltipBridge title={item.label} triggerRef={triggerRef} handlersRef={handlersRef} /></Focusable></TooltipTrigger>
  </Tab>
}

interface TabTooltipBridgeProps extends React.HTMLAttributes<Element> {
  title: string
  triggerRef: React.RefObject<HTMLDivElement | null>
  handlersRef: React.RefObject<React.HTMLAttributes<Element> | null>
}

/** RAC Focusable anchors to the existing Tab, never a nested focusable element. */
const TabTooltipBridge = React.forwardRef<HTMLDivElement, TabTooltipBridgeProps>(function TabTooltipBridge({ title, triggerRef, handlersRef, ...triggerProps }, forwardedRef) {
  React.useImperativeHandle(forwardedRef, () => triggerRef.current!, [triggerRef])
  React.useLayoutEffect(() => {
    handlersRef.current = triggerProps
    const tab = triggerRef.current
    const description = triggerProps['aria-describedby']
    if (description) tab?.setAttribute('aria-describedby', description)
    else tab?.removeAttribute('aria-describedby')
    return () => { if (handlersRef.current === triggerProps) handlersRef.current = null; tab?.removeAttribute('aria-describedby') }
  }, [handlersRef, triggerRef, triggerProps])
  return <Tooltip placement="bottom" offset={geo.tooltipOffset} {...stylex.props(styles.tooltip)}>{title}</Tooltip>
})

const styles = stylex.create({
  root: { position: 'relative', flex: '1 1 0', minWidth: 0, maxWidth: '100%', height: g.controlSm },
  list: { display: 'flex', alignItems: 'center', gap: s.xs2, height: '100%', width: '100%', minWidth: 0, overflowX: 'auto', overflowY: 'hidden', scrollbarWidth: 'none', scrollSnapType: 'x mandatory', scrollBehavior: 'auto' },
  tab: { position: 'relative', boxSizing: 'border-box', width: `var(--tab-cell-width, ${g.controlSm})`, height: g.controlSm, flexShrink: 0, paddingInline: s.sm, borderWidth: 0, borderRadius: r.control, backgroundColor: sf.transparent, color: tx.fgMuted, fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading, cursor: 'pointer', scrollSnapAlign: 'start', scrollSnapStop: 'always', opacity: { default: 1, ':is([data-tab-clipped])': 0 }, pointerEvents: { default: 'auto', ':is([data-tab-clipped])': 'none' }, ':hover': { color: tx.fg }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: ac.primary, outlineOffset: `calc(-1 * ${g.focusRing})` } },
  selected: { backgroundColor: sf.washSubtle, color: tx.fg },
  label: { position: 'absolute', insetBlock: 0, insetInline: s.sm, display: 'block', lineHeight: g.controlSm, overflow: 'hidden', whiteSpace: 'nowrap', textOverflow: 'ellipsis', visibility: { default: 'visible', ':is([data-icon-only])': 'hidden' }, pointerEvents: 'none' },
  icon: { position: 'absolute', inset: 0, display: 'grid', placeItems: 'center', visibility: { default: 'hidden', ':is([data-icon-only])': 'visible' }, pointerEvents: 'none' },
  metric: { position: 'fixed', top: 0, left: 0, height: 0, visibility: 'hidden', pointerEvents: 'none' },
  iconMetric: { width: g.controlSm },
  maxMetric: { width: g.tabMax },
  minimum: { width: 'max-content', whiteSpace: 'nowrap' },
  // RAC supplies placement only after measuring the real mounted anchor.
  tooltip: { visibility: { default: 'hidden', ':is([data-placement])': 'visible' }, zIndex: 50, borderRadius: r.control, backgroundColor: tx.fg, color: sf.raised, paddingInline: s.row, paddingBlock: s.sm, fontSize: t.denseSize, fontFamily: t.fontSans },
})
