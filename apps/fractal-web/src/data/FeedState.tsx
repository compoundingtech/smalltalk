import * as stylex from '@stylexjs/stylex'

import { EmptyState, Spinner } from '../ui-compat/components.tsx'
import { scale, tokens } from '../ui-compat/tokens.stylex.ts'

import type { Feed } from './source.ts'

/** Source states that have not supplied an observed value. */
export type UnobservedFeed = Exclude<Feed<unknown>, { readonly _tag: 'Observed' }>

const recovery = {
  ungranted: 'Ask the gateway owner to grant access to this surface. Your saved work is unchanged.',
  unsupported:
    'This gateway does not support this surface. Choose another surface; your saved work is unchanged.',
  failed:
    'The gateway could not load this surface. Reload to reconnect; your saved work is unchanged.',
} as const

/** Shared projection state; missing observations never replace the recipient's composer or outbox. */
export const FeedState = ({
  feed,
  label,
}: {
  readonly feed: UnobservedFeed
  readonly label: string
}) => (
  <section role="status" aria-label={`${label} availability`} {...stylex.props(styles.root)}>
    <EmptyState
      icon={feed._tag === 'Waiting' ? <Spinner size="sm" /> : undefined}
      title={
        feed._tag === 'Waiting'
          ? `Waiting for ${label}…`
          : label === 'Conversation'
            ? 'Transcript unavailable'
            : `${label} unavailable`
      }
      description={
        feed._tag === 'Waiting'
          ? 'The first observation has not arrived yet.'
          : recovery[feed.reason]
      }
    />
    {feed._tag === 'Unavailable' && (
      <details {...stylex.props(styles.diagnostic)}>
        <summary {...stylex.props(styles.summary)}>Connection details</summary>
        <p {...stylex.props(styles.detail)}>{feed.detail}</p>
      </details>
    )}
  </section>
)

const styles = stylex.create({
  root: {
    padding: scale.space4,
    display: 'flex',
    flexDirection: 'column',
    alignItems: 'center',
    gap: scale.space3,
    color: tokens['--ds-gray-1000'],
    fontFamily: scale.fontSans,
  },
  diagnostic: {
    width: '100%',
    maxWidth: '80ch',
    minWidth: 0,
    fontSize: '0.75rem',
    color: tokens['--ds-gray-900'],
  },
  summary: {
    cursor: 'pointer',
    borderRadius: scale.radiusSm,
    outlineStyle: { default: 'none', ':focus-visible': 'solid' },
    outlineWidth: 2,
    outlineColor: tokens['--ds-focus-color'],
    outlineOffset: 2,
  },
  detail: { marginBlock: scale.space2, whiteSpace: 'pre-wrap', overflowWrap: 'anywhere' },
})
