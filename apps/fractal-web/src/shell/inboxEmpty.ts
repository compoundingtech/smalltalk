import type { Attention } from '@smalltalk/st3-client/schema'

import type { Feed } from '../data/source.ts'

/**
 * What the inbox says when it has no rows. "Nothing needs you" is a claim about the fleet, so it is
 * made only from a live read: while the alerts are not loaded, are unavailable, or were last seen
 * before the connection lapsed, the inbox says so instead of an empty list reading as good news.
 */
export const inboxEmptyText = (feed: Feed<readonly Attention[]>): string => {
  switch (feed._tag) {
    case 'Waiting':
      return 'Checking for alerts…'
    case 'Unavailable':
      return `Alerts could not be loaded: ${feed.detail}`
    case 'Observed':
      return feed.freshness === 'stale' || feed.error !== undefined
        ? 'No alerts known — this view is out of date'
        : 'Nothing needs you'
  }
}
