import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { FocusScope } from 'react-aria'
import { borderVars as border, elevationVars as elevation, surfaceVars as surface, textVars as ink, typeVars as t } from '../composition-tokens.stylex'

export interface SidePanelProps {
  /** Landmark name; the panel is a labelled complementary region, never a dialog. */
  readonly 'aria-label': string
  /** Escape inside the panel calls this; the host unmounts the panel to close it. */
  readonly onClose: () => void
  readonly children?: React.ReactNode
  readonly id?: string
  readonly ref?: React.Ref<HTMLElement>
  readonly 'data-testid'?: string
  readonly style?: stylex.StyleXStyles
}

/**
 * A non-modal side panel floating over the trailing edge of the workspace. It moves focus into
 * itself on mount, closes on Escape and returns focus to the element focused before it opened,
 * but never contains Tab, hides, inerts or scroll-locks the rest of the app. Use a modal dialog
 * only when the user must finish or dismiss the task before anything else.
 */
export function SidePanel({ 'aria-label': label, onClose, children, id, ref, 'data-testid': testId, style }: SidePanelProps) {
  const own = React.useRef<HTMLElement | null>(null)
  const attach = React.useCallback((node: HTMLElement | null) => {
    own.current = node
    if (typeof ref === 'function') return ref(node)
    if (ref) ref.current = node
  }, [ref])
  // FocusScope focuses the first tabbable child; content still loading has none, so take focus here.
  React.useLayoutEffect(() => {
    const panel = own.current
    if (panel && !panel.contains(document.activeElement)) panel.focus({ preventScroll: true })
  }, [])
  const onKeyDown = (event: React.KeyboardEvent<HTMLElement>) => {
    // A focused child that handles Escape itself (menu, combobox) keeps the panel open.
    if (event.key !== 'Escape' || event.defaultPrevented) return
    event.preventDefault()
    event.stopPropagation()
    onClose()
  }
  return <FocusScope autoFocus restoreFocus>
    <aside ref={attach} id={id} tabIndex={-1} aria-label={label} data-side-panel="" data-testid={testId} onKeyDown={onKeyDown} {...stylex.props(styles.panel, style)}>{children}</aside>
  </FocusScope>
}

const styles = stylex.create({
  panel: {
    position: 'fixed', insetBlock: 0, insetInlineEnd: 0, zIndex: 100,
    display: 'flex', flexDirection: 'column', boxSizing: 'border-box',
    width: 380, maxWidth: 'calc(100vw - 24px)', minHeight: 0,
    backgroundColor: surface.raised, color: ink.fg, fontFamily: t.fontSans, fontSize: t.metaSize,
    borderInlineStartWidth: 1, borderInlineStartStyle: 'solid', borderInlineStartColor: border.borderStrong,
    boxShadow: elevation.dialog, outline: 'none',
  },
})
