// Loading, empty, offline, stale and error outside the conversation. Explicit-fact presentation only.
// One amber band under the thread header owns the gateway connection; panes keep quiet one-line rows,
// never repeat the connection, and only a pane's own error turns red.
import * as stylex from '@stylexjs/stylex'
import { Button } from 'react-aria-components'
import { accentVars as accent, borderVars as border, geometryVars as g, radiusVars as r, spaceVars as s, statusVars as tone, surfaceVars as surface, textVars as ink, typeVars as t } from '../composition-tokens.stylex'
import { Icon } from './Icons'

export type SystemStateKind = 'loading' | 'empty' | 'offline' | 'stale' | 'error'
/** `live` shows nothing. */
export type ConnectionState = 'live' | 'reconnecting' | 'offline' | 'stale' | 'failed'

type DataAttributes = { readonly [key: `data-${string}`]: string | undefined }

export interface SystemNoticeProps extends DataAttributes {
  readonly kind: SystemStateKind
  /** One short sentence-case statement of the state: "No missions", "Last observed roster". */
  readonly title: string
  /** What it means for you, in one line: "Live updates pending". */
  readonly detail?: string
  /** `pane` fills a pane body or side panel; `rail` sits in the sidebar. */
  readonly scope?: 'pane' | 'rail'
  /** The state comes from the shared gateway connection, which the connection banner owns: the row names only what it still shows. */
  readonly connection?: boolean
  readonly onRetry?: () => void
}

/** One quiet system-state row for a pane or the sidebar; `role="status"` sits on the statement. */
export function SystemNotice({ kind, title, detail, scope = 'pane', connection = false, onRetry, ...data }: SystemNoticeProps) {
  return <div data-system-state={kind} {...data} {...stylex.props(styles.quiet, scope === 'rail' ? styles.quietRail : styles.quietPane)}>
    <span role="status" {...stylex.props(styles.quietStatus, kind === 'error' && !connection && styles.toneError)}>
      {kind === 'loading' && <Icon name="spinner" size={12} spinning />}
      <span {...stylex.props(styles.lineText)}>{connection || detail === undefined ? title : `${title}. ${detail}`}</span>
    </span>
    {onRetry !== undefined && !connection && <Button aria-label={`Retry: ${title}`} onPress={onRetry} {...stylex.props(styles.lineButton, styles.focusable)}>Retry</Button>}
  </div>
}

export interface ConnectionStatusProps {
  readonly state: ConnectionState
  /**
   * The host's decoded detail. Reconnecting: the next try, e.g. "Next try in 2s · attempt 5". Failed: the safe reason,
   * e.g. "No access to connection · ask an administrator". Other states show what is still on screen.
   */
  readonly text?: string
  readonly onReconnect?: () => void
}

const connectionTitle: Readonly<Record<Exclude<ConnectionState, 'live'>, string>> = { offline: 'You are offline', reconnecting: 'Reconnecting', stale: 'Live updates paused', failed: 'Connection lost' }
const connectionDetail = 'Showing what was last observed'

/**
 * The shared gateway connection as one band under the thread header; nothing while live. The live region holds only
 * the state's headline, so a screen reader hears one announcement per transition, never the ticking countdown; the
 * detail sits beside it, readable but not announced.
 */
export function ConnectionStatus({ state, text, onReconnect }: ConnectionStatusProps) {
  if (state === 'live') return null
  const detail = (state === 'reconnecting' || state === 'failed') && text !== undefined ? text : connectionDetail
  return <div data-connection-state={state} {...stylex.props(styles.banner)}>
    <span {...stylex.props(styles.bannerLine)}>
      <span {...stylex.props(styles.bannerIcon, state === 'failed' ? styles.toneError : styles.toneWarning)}><Icon name="alert" size={14} /></span>
      <strong role="status" {...stylex.props(styles.bannerTitle)}>{connectionTitle[state]}</strong>
      <span aria-hidden="true" {...stylex.props(styles.toneMuted)}>·</span>
      <span data-connection-detail {...stylex.props(styles.bannerDetail)}>{detail}</span>
    </span>
    {onReconnect !== undefined && <Button onPress={onReconnect} {...stylex.props(styles.outlineButton, styles.focusable)}>Reconnect</Button>}
  </div>
}

const styles = stylex.create({
  focusable: { ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary, outlineOffset: g.hairline } },
  toneMuted: { color: ink.fgMuted },
  toneWarning: { color: tone.attention },
  toneError: { color: tone.dangerFg },
  quiet: { display: 'flex', alignItems: 'center', gap: s.xs, minWidth: 0, height: g.toolRow, flexShrink: 0, boxSizing: 'border-box', color: ink.fgMuted, fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading },
  quietPane: { paddingInline: s.lg, marginTop: s.md },
  quietRail: { paddingInline: s.lg },
  quietStatus: { display: 'flex', alignItems: 'center', gap: s.sm, flex: '1 1 0', minWidth: 0 },
  lineText: { display: 'block', flex: '1 1 0', minWidth: 0, overflow: 'hidden', whiteSpace: 'nowrap', textOverflow: 'ellipsis' },
  lineButton: { display: 'inline-flex', alignItems: 'center', height: g.toolRow, paddingInline: s.xs, flexShrink: 0, borderWidth: 0, borderRadius: r.sm, backgroundColor: surface.transparent, color: ink.fgMuted, fontFamily: t.fontSans, fontSize: t.denseSize, whiteSpace: 'nowrap', cursor: 'pointer', ':hover': { backgroundColor: surface.rowHover, color: ink.fg } },
  outlineButton: { display: 'inline-flex', alignItems: 'center', justifyContent: 'center', height: g.controlSm, paddingInline: s.md, flexShrink: 0, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.control, backgroundColor: surface.controlFill, color: ink.fg, fontFamily: t.fontSans, fontSize: t.metaSize, whiteSpace: 'nowrap', cursor: 'pointer', ':hover': { backgroundColor: surface.rowHover } },
  banner: { display: 'flex', alignItems: 'center', gap: s.md, minHeight: g.controlLg, paddingInline: s.lg, paddingBlock: s.xs, flexShrink: 0, boxSizing: 'border-box', borderBottomWidth: g.hairline, borderBottomStyle: 'solid', borderBottomColor: border.border, backgroundColor: `color-mix(in srgb, ${tone.attention} 10%, ${surface.canvas})`, color: ink.fg, fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading },
  bannerLine: { display: 'flex', alignItems: 'center', gap: s.sm, flex: '1 1 0', minWidth: 0 },
  bannerIcon: { display: 'inline-flex', flexShrink: 0 },
  bannerTitle: { flexShrink: 0, fontWeight: t.weightMedium },
  bannerDetail: { minWidth: 0, overflow: 'hidden', whiteSpace: 'nowrap', textOverflow: 'ellipsis', color: ink.fgSoft },
})
