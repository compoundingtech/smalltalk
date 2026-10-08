// Explicit-clock presentation only.
import * as React from 'react'
import { compactTime, type SidebarDuration as Duration } from './model'

/** Display the reported duration scope; a one-day activity span is never labelled lifetime.
 *  Only a reported duration renders: callers omit the field when the source reported none. */
export const SidebarDuration = React.memo(function SidebarDuration({ duration }: { readonly duration: Extract<Duration, { readonly _tag: 'Known' }> }) {
  const value = compactTime({ at: 0, now: duration.ms })
  const label = duration.scope === '24h-activity-span' ? '24h activity span' : 'Lifetime duration'
  return <time dateTime={`PT${duration.ms / 1000}S`} title={`${label}: ${duration.ms} milliseconds`} aria-label={`${label}: ${value}`}>{value}{duration.scope === '24h-activity-span' && <span data-line1-drop="scope">/24h</span>}</time>
})
