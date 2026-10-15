import * as React from 'react'
import * as Aria from 'react-aria-components'
import * as stylex from '@stylexjs/stylex'
import { ResizableSplit } from '@smalltalk/fractal-ui/assistant-ui/shell'
import { Icon } from '../../../../packages/fractal-ui/src/assistant-ui/composition/Icons.tsx'
import { colorVars as c, typeVars as t, spaceVars as s, geometryVars as g } from '../../../../packages/fractal-ui/src/assistant-ui/composition-tokens.stylex.ts'

export const sidebarMinimumWidth = 240
export const sidebarDrawerBreakpoint = 900

export type ResponsiveAgentSidebarProps = {
  readonly viewport: number
  readonly width: number
  readonly maxWidth: number
  readonly collapsed: boolean
  readonly drawerOpen: boolean
  readonly onDrawerOpenChange: (open: boolean) => void
  readonly onToggleCollapsed: () => void
  readonly onWidthChange: (width: number) => void
  readonly onWidthCommit: (width: number) => void
  readonly onWidthReset: () => void
  readonly children: React.ReactNode
}

// Dialog filters aria-modal from its props; its DOM renderer preserves the modal declaration.
const renderModalDialog = (props: React.ComponentPropsWithRef<'section'>) => <section {...props} aria-modal="true" />

/** The dock consumes layout space; the modal drawer never resizes the thread. */
export const ResponsiveAgentSidebar = ({ viewport, width, maxWidth, collapsed, drawerOpen, onDrawerOpenChange, onToggleCollapsed, onWidthChange, onWidthCommit, onWidthReset, children }: ResponsiveAgentSidebarProps) => {
  const portalContainer = React.useRef<HTMLDivElement>(null)
  const narrow = viewport < sidebarDrawerBreakpoint
  const sidebar = <aside aria-label="Agents" data-sidebar-presentation={narrow ? 'drawer' : 'dock'} style={{ width: Math.max(sidebarMinimumWidth, width) }} {...stylex.props(styles.sidebar)}>
    <header {...stylex.props(styles.brand)}>
      <Aria.Button autoFocus={narrow} aria-label={narrow ? 'Close agents' : 'Collapse agents'} onPress={() => narrow ? onDrawerOpenChange(false) : onToggleCollapsed()} {...stylex.props(styles.iconButton)}><Icon name="chevrons-left" /></Aria.Button>
      <strong {...stylex.props(styles.wordmark)}>Fractal</strong>
    </header>
    {children}
  </aside>
  if (!narrow) return collapsed ? null : <>{sidebar}<ResizableSplit id="live-agent-sidebar" value={width} min={sidebarMinimumWidth} max={maxWidth} collapsed={false} label="Agent sidebar width" onChange={onWidthChange} onCommit={onWidthCommit} onToggle={onToggleCollapsed} onReset={onWidthReset} /></>
  return <div ref={portalContainer} {...stylex.props(styles.portal)}>
    <Aria.ModalOverlay isOpen={drawerOpen} onOpenChange={onDrawerOpenChange} isDismissable UNSTABLE_portalContainer={portalContainer.current ?? undefined} {...stylex.props(styles.overlay)}>
      <Aria.Modal {...stylex.props(styles.modal)}>
        <Aria.Dialog aria-label="Agent navigation" render={renderModalDialog} {...stylex.props(styles.dialog)}>{sidebar}</Aria.Dialog>
      </Aria.Modal>
    </Aria.ModalOverlay>
  </div>
}

/** One sidebar chord for the dock and drawer, without stealing text or terminal input. */
export const useSidebarShortcut = (toggle?: () => void): void => {
  React.useLayoutEffect(() => {
    if (toggle === undefined) return
    const keydown = (event: KeyboardEvent) => {
      if (event.defaultPrevented || event.repeat || event.altKey || event.shiftKey || !(event.metaKey || event.ctrlKey) || event.key.toLowerCase() !== 'b') return
      const target = event.target
      if (target instanceof Element && target.closest('input, textarea, select, [contenteditable]:not([contenteditable="false"]), [role="textbox"]') !== null) return
      event.preventDefault()
      toggle()
    }
    document.addEventListener('keydown', keydown)
    return () => document.removeEventListener('keydown', keydown)
  }, [toggle])
}

const styles = stylex.create({
  sidebar: { display: 'flex', flexDirection: 'column', flexShrink: 0, minWidth: sidebarMinimumWidth, minHeight: 0, height: '100%', overflow: 'hidden', backgroundColor: c.sidebar, color: c.fg },
  brand: { height: g.band, paddingInline: s.md, display: 'flex', alignItems: 'center', gap: s.md, flexShrink: 0 },
  wordmark: { fontWeight: 500 },
  iconButton: { width: 28, height: 28, display: 'inline-flex', justifyContent: 'center', alignItems: 'center', backgroundColor: 'transparent', color: c.fgMuted, borderWidth: 0, borderRadius: 6, cursor: 'pointer', ':focus-visible': { outline: `2px solid ${c.primary}`, outlineOffset: 2 } },
  portal: { display: 'contents' },
  overlay: { position: 'fixed', insetInline: 0, insetBlockStart: 0, insetBlockEnd: 'var(--wf-devbar-space, 0px)', zIndex: 90, display: 'flex', backgroundColor: 'rgba(0, 0, 0, 0.4)' },
  modal: { height: '100%', minWidth: sidebarMinimumWidth, backgroundColor: c.sidebar, color: c.fg, fontFamily: t.fontSans, fontSize: t.uiSize, lineHeight: t.uiLeading },
  dialog: { height: '100%', display: 'flex', outline: 'none' },
})
