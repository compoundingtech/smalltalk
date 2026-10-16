import type * as Native from '@smalltalk/st3-client/schema'
import * as stylex from '@stylexjs/stylex'
import { DateTime } from 'effect'
import * as React from 'react'
import * as Aria from 'react-aria-components'

import { alertConversations, alertsHeading, homeKind, openAlerts, TIERS } from '../../../../clients/typescript/st3-views/homeView.ts'
import { ago } from '../../../../clients/typescript/st3-views/time.ts'
import { colorVars as c, spaceVars as s, typeVars as t } from '../../../../packages/fractal-ui/src/assistant-ui/composition-tokens.stylex.ts'
import { useAttention } from '../data/react.tsx'
import type { Feed } from '../data/source.ts'

// Every open alert across conversations, behind a count, as stui's top bar shows it: nothing at
// zero, `N alerts` otherwise. It reads the attention collection the conversation strips read, so
// the count, the list and each conversation agree. A row opens the conversation that answers it.

const KIND_WORDS: Record<string, string> = {
  review: 'review',
  launch: 'launch',
  revision: 'revision',
  request: 'request',
  prompt: 'prompt',
  login: 'sign-in',
}

export type AlertsListRow = {
  readonly id: string
  readonly title: string
  readonly kind: string
  readonly from: string
  readonly age: string
  /** The agent conversation that answers it, when st names one. */
  readonly conversation?: string
}

/** The open alerts in Home's order (who is stopped first), st's order within each tier. */
export const alertsList = (items: readonly Native.Attention[], now: number): readonly AlertsListRow[] =>
  openAlerts(items, undefined)
    .map((item, index) => ({ item, index, place: homeKind(item)! }))
    .sort((a, b) => TIERS.indexOf(a.place.tier) - TIERS.indexOf(b.place.tier) || a.index - b.index)
    .map(({ item, place }) => {
      const conversation = alertConversations(item).find((ref) => ref.startsWith('agent/'))
      return {
        id: item.id,
        title: item.title,
        kind: KIND_WORDS[place.kind] ?? place.kind,
        from: (item.requester_id ?? conversation ?? item.source_id).replace(/^agent\//, ''),
        age: ago(DateTime.formatIso(item.requested_at), now),
        ...(conversation === undefined ? {} : { conversation }),
      }
    })

/** The count's words: nothing at zero; unknown is never shown as none. */
export const alertsCount = (feed: Feed<readonly Native.Attention[]>, rows: readonly AlertsListRow[]): string =>
  feed._tag === 'Unavailable' ? 'alerts ?' : feed._tag === 'Waiting' ? '' : `${alertsHeading(rows.length)}${rows.length > 0 && feed.freshness === 'stale' ? ' · stale' : ''}`

export const AlertsPopover = ({
  onOpen,
  now = Date.now(),
}: {
  /** Opens an agent's conversation. */
  readonly onOpen: (ref: string) => void
  readonly now?: number
}): React.ReactNode => {
  const feed = useAttention()
  const rows = React.useMemo(() => (feed._tag === 'Observed' ? alertsList(feed.value, now) : []), [feed, now])
  const [open, setOpen] = React.useState(false)
  const count = alertsCount(feed, rows)
  if (count === '') return null
  return (
    <Aria.DialogTrigger isOpen={open} onOpenChange={setOpen}>
      <Aria.Button
        aria-label={feed._tag === 'Unavailable' ? 'Alerts could not be loaded' : count}
        {...stylex.props(styles.count, feed._tag === 'Unavailable' && styles.unknown)}
      >
        {feed._tag === 'Unavailable' ? count : `◆ ${count}`}
      </Aria.Button>
      <Aria.Popover placement="bottom start" {...stylex.props(styles.popover)}>
        <Aria.Dialog aria-label="Alerts" {...stylex.props(styles.dialog)}>
          {feed._tag === 'Unavailable' ? (
            <p {...stylex.props(styles.empty)}>
              {feed.reason === 'ungranted' ? 'Read access to alerts has not been granted.'
                : feed.reason === 'unsupported' ? 'This connection does not support alerts.'
                  : 'Alerts could not be loaded; reconnect and try again.'}
            </p>
          ) : (
            <ul {...stylex.props(styles.list)}>
              {rows.map((row) => (
                <li key={row.id}>
                  <Aria.Button
                    isDisabled={row.conversation === undefined}
                    onPress={() => {
                      if (row.conversation === undefined) return
                      setOpen(false)
                      onOpen(row.conversation)
                    }}
                    {...stylex.props(styles.row)}
                  >
                    <span {...stylex.props(styles.title)}>◆ {row.title}</span>
                    <span {...stylex.props(styles.meta)}>{row.kind} · {row.from} · {row.age}</span>
                  </Aria.Button>
                </li>
              ))}
            </ul>
          )}
        </Aria.Dialog>
      </Aria.Popover>
    </Aria.DialogTrigger>
  )
}

const styles = stylex.create({
  count: {
    padding: '2px 8px',
    borderWidth: 0,
    borderRadius: 6,
    backgroundColor: 'transparent',
    color: c.attention,
    fontSize: t.metaSize,
    fontWeight: 600,
    cursor: 'pointer',
    whiteSpace: 'nowrap',
    ':hover': { backgroundColor: c.rowHover },
    ':focus-visible': { outline: `2px solid ${c.primary}` },
  },
  unknown: { color: c.fgMuted, fontWeight: 400 },
  popover: { backgroundColor: c.raised, color: c.fg, border: `1px solid ${c.border}`, borderRadius: 6, fontFamily: t.fontSans, fontSize: t.metaSize, zIndex: 100, maxWidth: 'min(420px, calc(100vw - 32px))' },
  dialog: { outline: 'none', maxHeight: '60vh', overflowY: 'auto', padding: s.xs },
  list: { margin: 0, padding: 0, listStyle: 'none', display: 'flex', flexDirection: 'column', gap: 2 },
  row: {
    width: '100%',
    display: 'flex',
    flexDirection: 'column',
    alignItems: 'flex-start',
    gap: 2,
    padding: s.md,
    borderWidth: 0,
    borderRadius: 4,
    backgroundColor: 'transparent',
    color: c.fg,
    textAlign: 'start',
    cursor: 'pointer',
    ':hover': { backgroundColor: c.rowHover },
    ':focus-visible': { outline: `2px solid ${c.primary}` },
    ':disabled': { cursor: 'default' },
  },
  title: { fontWeight: 600, overflowWrap: 'anywhere' },
  meta: { color: c.fgMuted, fontSize: t.denseSize },
  empty: { margin: 0, padding: s.md, color: c.fgMuted },
})
