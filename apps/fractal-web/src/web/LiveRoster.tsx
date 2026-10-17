import * as React from 'react'
import * as Aria from 'react-aria-components'
import * as stylex from '@stylexjs/stylex'
import type { Undecodable } from '@st3/sdk/effect'
import { SidebarAgentRow } from '@smalltalk/fractal-ui/assistant-ui/shell'
import { colorVars as c, typeVars as t, spaceVars as s, geometryVars as g } from '../../../../packages/fractal-ui/src/assistant-ui/composition-tokens.stylex.ts'
import type { Agent, Feed, Fleet, RosterDecode } from '../data/source.ts'
import { sidebarRow } from './sidebarRow.ts'

/** Roster rows the latest decode could not show, never rows shown with an omitted open leaf. */
export interface HiddenRoster {
  readonly count: number
  readonly failures: readonly Undecodable[]
}

/**
 * Rows count once: a typed identity groups its field failures; an id-less row reports exactly one
 * identity (`id`) or whole-row (`$`) failure. Degraded leaves never count as hidden agents.
 */
export const hiddenRoster = ({ decode, shown }: {
  readonly decode: RosterDecode
  readonly shown: ReadonlySet<string>
}): HiddenRoster => {
  const failures = decode.undecodable.filter((entry) =>
    entry._tag === 'Rejected' && (decode.decodeStale || entry.id === undefined || !shown.has(entry.id)))
  const ids = new Set<string>()
  let anonymous = 0
  for (const entry of failures) {
    if (entry.id !== undefined) ids.add(entry.id)
    else if (entry.path === 'id' || entry.path === '$') anonymous += 1
  }
  const count = ids.size + anonymous
  return { count: count === 0 && failures.length > 0 ? 1 : count, failures }
}

export const rosterDecodeNotice = (count: number) =>
  `${count} ${count === 1 ? 'agent' : 'agents'} could not be shown · this view is older than the daemon`

/** One quiet roster notice; the disclosure carries the SDK's path and reason per failure. */
export function RosterDecodeNotice({ decode, shown }: {
  readonly decode: RosterDecode
  readonly shown: ReadonlySet<string>
}) {
  const hidden = hiddenRoster({ decode, shown })
  if (hidden.count === 0) return null
  return (
    <div data-wf-roster-decode={decode.decodeStale ? 'stale' : 'partial'} {...stylex.props(styles.notice, styles.decode)}>
      {/* Polite and atomic: only a changed count is announced, never per-row failures. */}
      <p role="status" aria-live="polite" aria-atomic="true" {...stylex.props(styles.sentence)}>{rosterDecodeNotice(hidden.count)}</p>
      <Aria.Disclosure>
        <Aria.Button slot="trigger" {...stylex.props(styles.disclosure)}>Details</Aria.Button>
        <Aria.DisclosurePanel>
          <ul tabIndex={0} aria-label="Agents that could not be shown" {...stylex.props(styles.failures)}>
            {hidden.failures.map((failure, index) => (
              <li key={`${failure.id ?? ''}:${failure.path}:${index}`} data-wf-decode-path={failure.path}>
                <span {...stylex.props(styles.identity)}>{failure.id ?? 'Row without a readable id'}</span>
                {' · '}
                <code {...stylex.props(styles.path)}>{failure.path}</code>
                {' · '}
                {failure.reason}
              </li>
            ))}
          </ul>
        </Aria.DisclosurePanel>
      </Aria.Disclosure>
    </div>
  )
}

/** The live sidebar's agent roster: rows plus its one availability or freshness notice. */
export function LiveRoster({ fleet, rows, stale, now, query, current, decode, onSelect, navRef, onHoverOwner }: {
  readonly fleet: Feed<Fleet>
  /** Filtered rows in display order. */
  readonly rows: readonly Agent[]
  readonly stale: boolean
  readonly now: number
  readonly query: string
  readonly current: string
  readonly decode: RosterDecode
  readonly onSelect: (ref: string) => void
  readonly navRef?: React.Ref<HTMLElement>
  readonly onHoverOwner?: (event: React.SyntheticEvent<HTMLElement>) => void
}) {
  const shown = React.useMemo(
    () => new Set(fleet._tag === 'Observed' ? fleet.value.agents.map((agent) => agent.ref) : []),
    [fleet],
  )
  return (
    <>
      <nav ref={navRef} aria-label="Agent roster" onMouseOverCapture={onHoverOwner} onFocusCapture={onHoverOwner} {...stylex.props(styles.roster)}>
        {fleet._tag === 'Waiting' ? <div aria-hidden="true" data-wf-roster-skeleton="waiting" {...stylex.props(styles.rosterSkeleton)}>
          {Array.from({ length: 8 }, (_, index) => <div key={index} data-wf-skeleton-row="" {...stylex.props(styles.skeletonRow)}>
            <span {...stylex.props(styles.skeletonGlyph)} />
            <span data-wf-skeleton-line="name" {...stylex.props(styles.skeletonLine, styles.skeletonName)} />
            <span data-wf-skeleton-line="host" {...stylex.props(styles.skeletonLine, styles.skeletonHost)} />
          </div>)}
        </div> : null}
        {rows.map((row) => (
          <SidebarAgentRow key={row.ref} item={sidebarRow({ agent: row, stale, now })} now={now} variant="SR-2" layout="SR2-A" glyph="SG-1" extraSignals={[]} query={query} active={current === row.ref} onOpen={() => onSelect(row.ref)} />
        ))}
      </nav>
      {fleet._tag !== 'Observed' ? (
        <p role="status" data-wf-roster-reason={fleet._tag === 'Unavailable' ? fleet.reason : undefined} {...stylex.props(styles.notice)}>
          {fleet._tag === 'Waiting' ? 'Waiting for the agent roster.' : fleet.reason === 'ungranted' ? 'Read access to the agent roster has not been granted.' : fleet.reason === 'unsupported' ? 'This connection does not support the agent roster.' : 'The agent roster could not be loaded; reconnect and try again.'}
        </p>
      ) : stale ? (
        <p role="status" data-wf-roster-stale={decode.decodeStale ? 'decode' : 'sync'} {...stylex.props(styles.notice)}>
          {fleet.coverage?._tag === 'Partial' ? 'Showing a partial roster · live updates pending' : 'Last observed roster · live updates pending'}
        </p>
      ) : null}
      {fleet._tag === 'Observed' ? <RosterDecodeNotice decode={decode} shown={shown} /> : null}
    </>
  )
}

const styles = stylex.create({
  roster: { display: 'flex', flexDirection: 'column', overflowY: 'auto', padding: s.md, minHeight: 0, flexGrow: 1, gap: 2 },
  // Match SR-2's shared row/control/icon geometry and the roster's 2px inter-row gap.
  rosterSkeleton: { display: 'flex', flexDirection: 'column', gap: 2 },
  skeletonRow: { height: g.sidebarRow, flexShrink: 0, display: 'grid', gridTemplateColumns: `${g.icon} minmax(0, 1fr)`, gridTemplateRows: `${g.controlSm} ${g.controlSm}`, alignContent: 'center', alignItems: 'center', columnGap: s.xs2, paddingInline: s.sm, boxSizing: 'border-box' },
  skeletonGlyph: { gridColumn: '1', gridRow: '1', width: g.icon, height: g.icon, borderRadius: '50%', backgroundColor: c.border, opacity: 0.5 },
  skeletonLine: { gridColumn: '2', height: 8, borderRadius: 4, backgroundColor: c.border, opacity: 0.5 },
  skeletonName: { gridRow: '1', width: '68%' },
  skeletonHost: { gridRow: '2', width: '44%' },
  notice: { padding: s.lg, fontSize: t.metaSize, color: c.fgMuted },
  decode: { display: 'flex', flexDirection: 'column', gap: s.xs, paddingBlockStart: 0, flexShrink: 0 },
  sentence: { margin: 0 },
  disclosure: { alignSelf: 'flex-start', padding: 0, borderWidth: 0, backgroundColor: 'transparent', color: c.fgMuted, fontSize: t.metaSize, textDecoration: 'underline', cursor: 'pointer', outline: { default: 'none', ':focus-visible': `2px solid ${c.primary}` }, outlineOffset: 2 },
  failures: { margin: 0, paddingInlineStart: s.lg, display: 'flex', flexDirection: 'column', gap: s.xs, maxHeight: 160, overflowY: 'auto', overflowWrap: 'anywhere' },
  identity: { color: c.fg },
  path: { fontFamily: t.fontMono, fontSize: t.denseSize },
})
