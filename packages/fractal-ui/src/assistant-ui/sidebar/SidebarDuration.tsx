// Explicit-clock presentation only.
import * as React from 'react'
import { compactTime, type SidebarAgentRow } from './model'

/** Display the reported duration scope; a one-day activity span is never labelled lifetime. */
export const SidebarDuration = React.memo(function SidebarDuration({ row }: { readonly row: SidebarAgentRow }) {
  const duration = row.duration
  if (duration._tag === 'Unknown') return <span title="Duration unknown" aria-label="Duration unknown">—</span>
  const value = compactTime({ at: 0, now: duration.ms })
  const label = duration.scope === '24h-activity-span' ? '24h activity span' : 'Lifetime duration'
  return <time dateTime={`PT${duration.ms / 1000}S`} title={`${label}: ${duration.ms} milliseconds`} aria-label={`${label}: ${value}`}>{value}{duration.scope === '24h-activity-span' ? '/24h' : ''}</time>
})
