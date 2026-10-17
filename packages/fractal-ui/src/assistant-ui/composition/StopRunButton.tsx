import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { accentVars as accent, borderVars as border, geometryVars as g, radiusVars as r, spaceVars as s, surfaceVars as surface, textVars as text, typeVars as t } from '../composition-tokens.stylex'
import { Icon } from './Icons'

/**
 * The composer's Stop pill beside Send. `pending` keeps it focusable but inert until the run ends.
 * Escape stops too; the key is announced through `aria-keyshortcuts`, not drawn. A native button,
 * because React Aria's Button drops `aria-keyshortcuts`.
 */
export function StopRunButton({ onStop, pending = false }: { readonly onStop: () => void; readonly pending?: boolean }) {
  const buttonRef = React.useCallback((button: HTMLButtonElement | null) => {
    if (!button) return
    return () => {
      // Hand keyboard focus back before the focused Stop leaves the composer.
      if (button.ownerDocument.activeElement !== button) return
      const form = button.closest('form')
      form?.querySelector<HTMLElement>('textarea, [role="textbox"]')?.focus()
      // A read-only conversation keeps Message disabled; keep focus in its composer instead.
      if (button.ownerDocument.activeElement === button) form?.focus()
    }
  }, [])
  return <button
    type="button"
    ref={buttonRef}
    data-testid="run-stop"
    aria-label={pending ? 'Stopping run' : 'Stop run'}
    aria-keyshortcuts="Escape"
    aria-disabled={pending}
    onClick={() => { if (!pending) onStop() }}
    {...stylex.props(styles.button, pending && styles.pending)}
  >
    <span aria-hidden="true" {...stylex.props(styles.mark, pending && styles.markPending)}><Icon name="stop" size={8} /></span>
    <span>{pending ? 'Stopping…' : 'Stop'}</span>
  </button>
}

const styles = stylex.create({
  button: {
    display: 'inline-flex', alignItems: 'center', gap: s.sm, flexShrink: 0, boxSizing: 'border-box',
    minHeight: g.controlMd, paddingInlineStart: s.sm, paddingInlineEnd: s.md,
    borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.full,
    backgroundColor: { default: surface.controlFill, ':hover': surface.rowHover }, color: text.fg,
    fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading, fontWeight: t.weightMedium, whiteSpace: 'nowrap',
    cursor: 'pointer',
    transition: 'background-color 150ms ease-out',
    ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary, outlineOffset: g.focusOffset },
  },
  pending: { cursor: 'default', opacity: 0.72 },
  // A filled square inside a disc, legible at 16px.
  mark: {
    display: 'inline-flex', alignItems: 'center', justifyContent: 'center', width: g.icon, height: g.icon, flexShrink: 0,
    borderRadius: r.full, backgroundColor: text.fg, color: surface.canvas,
  },
  markPending: { backgroundColor: text.fgMuted },
})
