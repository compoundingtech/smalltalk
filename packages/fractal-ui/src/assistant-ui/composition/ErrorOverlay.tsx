/**
 * Dismissible failure banner. Floating hosts portal notices into one layer;
 * lane hosts render them in place beside the failed work without covering prose.
 * Escape dismisses the newest inline or floating banner without moving focus,
 * unless a nested React Aria overlay consumed the key (defaultPrevented).
 * A new failure id shows again after a dismissal.
 */
import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { createPortal } from 'react-dom'
import { Button } from 'react-aria-components'
import { surfaceVars as surface, textVars as ink, borderVars as border, statusVars as tone, typeVars as t, radiusVars as r, spaceVars as s, geometryVars as g, elevationVars as elevation } from '../composition-tokens.stylex'
import { Icon } from './Icons'

export interface ErrorOverlayNotice {
  readonly id: string
  readonly title: string
  readonly detail?: string
  readonly onRetry?: () => void
  readonly onOpenOutput?: () => void
}

interface ErrorOverlayHostState {
  readonly layer: HTMLElement | null
  readonly dismissedIds: readonly string[]
  readonly lane: boolean
}

const ErrorOverlaySurface = React.createContext<ErrorOverlayHostState | null>(null)
/** The nearest host state; null when no host wraps the surface. */
export function useErrorOverlaySurface() {
  return React.useContext(ErrorOverlaySurface)
}

export function ErrorOverlayHost({ children, lane = false }: { readonly children: React.ReactNode; readonly lane?: boolean }) {
  const [layer, setLayer] = React.useState<HTMLDivElement | null>(null)
  const [dismissedIds, setDismissedIds] = React.useState<readonly string[]>([])
  const host = React.useMemo(() => ({ layer, dismissedIds, lane }), [layer, dismissedIds, lane])
  const onDismissKey = (event: React.KeyboardEvent<HTMLDivElement>) => {
    if (event.key !== 'Escape' || event.defaultPrevented || layer === null) return
    const banners = event.currentTarget.querySelectorAll<HTMLElement>('[data-error-overlay-id]')
    const newest = banners[banners.length - 1]
    if (newest === undefined) return
    event.preventDefault()
    event.stopPropagation()
    const id = newest.dataset.errorOverlayId!
    setDismissedIds(previous => previous.includes(id) ? previous : [...previous, id])
  }
  return <div data-error-overlay-host onKeyDown={onDismissKey} {...stylex.props(styles.host)}>
    <ErrorOverlaySurface.Provider value={host}>{children}</ErrorOverlaySurface.Provider>
    <div ref={setLayer} data-error-overlay-layer {...stylex.props(styles.layer)} />
  </div>
}

export function ErrorOverlay({ id, title, detail, onRetry, onOpenOutput }: ErrorOverlayNotice) {
  const host = React.useContext(ErrorOverlaySurface)
  const [dismissedId, setDismissedId] = React.useState<string | null>(null)
  if (dismissedId === id || host?.dismissedIds.includes(id)) return null
  if (host !== null && host.layer === null) return null
  const banner = <div role="alert" data-error-overlay data-error-overlay-id={id} onKeyDown={event => { if (host === null && event.key === 'Escape' && !event.defaultPrevented) { event.preventDefault(); setDismissedId(id) } }} {...stylex.props(styles.banner, host?.lane === true && styles.bannerLane)}>
    <Icon name="alert" size={14} />
    <span {...stylex.props(styles.text)}><strong {...stylex.props(styles.title)}>{title}</strong>{detail !== undefined && <span {...stylex.props(styles.detail)}>{detail}</span>}</span>
    {onRetry !== undefined && <Button onPress={onRetry} {...stylex.props(styles.action)}>Retry</Button>}
    {onOpenOutput !== undefined && <Button onPress={onOpenOutput} {...stylex.props(styles.action)}>Open output</Button>}
    <Button aria-label={`Dismiss: ${title}`} onPress={() => setDismissedId(id)} {...stylex.props(styles.close)}><Icon name="x" size={12} /></Button>
  </div>
  return host === null || host.lane || host.layer === null ? banner : createPortal(banner, host.layer)
}

const styles = stylex.create({
  host: { position: 'relative', display: 'flex', flexDirection: 'column', flex: '1 1 0', minHeight: 0, minWidth: 0 },
  layer: { position: 'absolute', insetInline: 0, top: 0, display: 'flex', flexDirection: 'column', alignItems: 'flex-end', gap: s.xs, paddingBlockStart: s.sm, paddingInline: s.lg, boxSizing: 'border-box', pointerEvents: 'none', zIndex: 30 },
  banner: { pointerEvents: 'auto', display: 'flex', alignItems: 'center', gap: s.sm, maxWidth: `min(100%, ${g.specimenAside})`, boxSizing: 'border-box', padding: s.md, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.md, backgroundColor: surface.raised, boxShadow: elevation.popover, color: ink.fg, fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading },
  bannerLane: { maxWidth: '100%', marginBlockEnd: s.sm },
  text: { display: 'flex', flexDirection: 'column', minWidth: 0, flex: '1 1 0' },
  title: { color: tone.dangerFg, fontWeight: t.weightSemibold },
  detail: { color: ink.fgSoft, overflowWrap: 'anywhere' },
  action: { display: 'inline-flex', alignItems: 'center', justifyContent: 'center', minHeight: g.controlSm, paddingInline: s.sm, flexShrink: 0, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.sm, backgroundColor: surface.controlFill, color: ink.fg, fontFamily: t.fontSans, fontSize: t.metaSize, cursor: 'pointer', ':hover': { backgroundColor: surface.rowHover }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: tone.danger } },
  close: { display: 'inline-flex', alignItems: 'center', justifyContent: 'center', width: g.controlSm, minHeight: g.controlSm, padding: 0, borderWidth: 0, borderRadius: r.sm, backgroundColor: surface.transparent, color: ink.fgMuted, cursor: 'pointer', flexShrink: 0, ':hover': { color: ink.fg, backgroundColor: surface.rowActive }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: tone.danger } },
})
