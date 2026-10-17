import * as React from 'react'
import { VisuallyHidden } from 'react-aria-components'
import { compactTime } from './model'
import { useClockLabel } from '../../time/clock'

export type SidebarClock = { readonly now?: number }
type TimeProps = { readonly at: number; readonly kind?: 'status' | 'activity' | 'turn'; readonly compact?: boolean; readonly showKind?: boolean }
const TimeValue = React.memo(function TimeValue({ at, now, kind = 'status', compact = false, showKind = false }: TimeProps & SidebarClock) {
  const iso = React.useMemo(() => new Date(at).toISOString(), [at])
  const value = useClockLabel(now => at <= now ? compactTime({ at, now }) : '', now)
  const label = kind === 'turn' ? 'Last completed turn' : kind === 'activity' ? 'Last activity' : 'Observed status since'
  const descriptionId = React.useId()
  if (value === '') return null
  // The exact ISO instant stays available as the description; hosts own the only hover disclosure.
  return <><time dateTime={iso} aria-describedby={descriptionId} aria-label={`${kind === 'status' ? 'Elapsed in status' : label}: ${value}${kind === 'status' ? '' : ' ago'}`}>{showKind && kind === 'activity' ? 'act' : ''}{value}{kind !== 'status' && !compact ? ' ago' : ''}</time><VisuallyHidden elementType="span"><span id={descriptionId}>{`${label} ${iso}`}</span></VisuallyHidden></>
})
/** A fixed host clock is deterministic; omitting it subscribes only this time label to context. */
export const SidebarTime = React.memo(function SidebarTime({ now, ...props }: TimeProps & SidebarClock) { return <TimeValue {...props} now={now} /> })
