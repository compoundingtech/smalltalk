import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { Tooltip, TooltipTriggerStateContext, type TooltipTriggerState } from 'react-aria-components'
import { useHover } from 'react-aria'
import { geometryNumbers } from '../assistant-ui/composition-tokens.stylex'
import { tooltipStyles } from '../assistant-ui/composition/Controls'
import { ThemePortal } from '../assistant-ui/taste/ThemePortal'
import { formatClock, formatDay, formatDayValue, formatDuration, formatExact, formatExactDay, formatRelative, type HumanTimeZone } from './human-time'
import { useClockLabel, useHumanTimeZone } from './clock'

export { HumanClockProvider, fixedClock, systemClock, useClockLabel, useHumanTimeZone, type HumanClock } from './clock'

type TimeKind = 'relative' | 'clock' | 'elapsed' | 'day'
interface HumanTimeProps {
  /** Epoch milliseconds of the instant. */
  readonly at: number
  /** `relative` age (default), `clock` header time, `elapsed` duration, or civil `day`. */
  readonly kind?: TimeKind
  /** Elapsed precision; `1` keeps only the largest unit for compact columns. */
  readonly maxUnits?: 1 | 2
  /** Visible text before/after the label, e.g. `Working for ` or ` in status`. */
  readonly prefix?: string
  readonly suffix?: string
  /** Show the exact timestamp in a hover tooltip; off inside rows that own their own hover card. */
  readonly tooltip?: boolean
  /** Milliseconds of hover before the tooltip opens; defaults to 400. */
  readonly hoverDelay?: number
  /** Accessible name prefix; the visible text follows it, and exact text remains the description. */
  readonly 'aria-label'?: string
  readonly 'data-testid'?: string
  readonly style?: stylex.StyleXStyles
}

const labelFor = (kind: TimeKind, at: number, now: number, zone: HumanTimeZone, maxUnits: 1 | 2): string =>
  kind === 'day' ? formatDay(at, now, zone) : kind === 'clock' ? formatClock(at, now, zone) : kind === 'elapsed' ? formatDuration(now - at, { maxUnits }) : formatRelative(at, now, zone)

/**
 * A self-updating time label (leaf clock). Renders `<time>` with the machine instant, the readable
 * label, and the exact local timestamp as accessible description and optional tooltip.
 */
export const HumanTime = React.memo(function HumanTime({ at, kind = 'relative', maxUnits = 2, prefix = '', suffix = '', tooltip = true, hoverDelay = 400, style, ...aria }: HumanTimeProps) {
  const label = useClockLabel((now, zone) => labelFor(kind, at, now, zone, maxUnits))
  const zone = useHumanTimeZone()
  const exact = Number.isFinite(at) ? kind === 'day' ? formatExactDay(at, zone) : formatExact(at, zone) : undefined
  const exactText = exact === undefined ? undefined : kind === 'elapsed' ? `Since ${exact}` : exact
  const visibleLabel = `${prefix}${label}${suffix}`
  const ref = React.useRef<HTMLTimeElement>(null)
  const tooltipEnabled = tooltip && exactText !== undefined
  const tip = useHoverTooltip(tooltipEnabled, hoverDelay)
  // While the tooltip is mounted the anchor names it as its description; the text equals
  // aria-description, which aria-describedby supersedes, so the description is announced once.
  const tooltipId = React.useId()
  const describedBy = tooltipEnabled && tip.state.isOpen ? tooltipId : undefined
  // RAC's Tooltip forwards `id` through its DOM-prop filter but leaves it out of TooltipProps.
  const tooltipDom: { readonly id: string } = { id: tooltipId }
  const time = <time
      ref={ref}
      dateTime={exact === undefined ? undefined : kind === 'day' ? formatDayValue(at, zone) : new Date(at).toISOString()}
      aria-label={aria['aria-label'] === undefined ? undefined : `${aria['aria-label']} ${visibleLabel}`}
      aria-description={exactText}
      aria-describedby={describedBy}
      data-testid={aria['data-testid']}
      data-human-time={kind}
      {...tip.hoverProps}
      {...stylex.props(styles.time, style)}
    >{visibleLabel}</time>
  if (!tooltipEnabled) return time
  return <ThemePortal>
    {time}
    {tip.state.isOpen && <TooltipTriggerStateContext.Provider value={tip.state}>
      <Tooltip {...tooltipDom} triggerRef={ref} placement="top" offset={geometryNumbers.tooltipOffset} {...stylex.props(tooltipStyles.tooltip, styles.tooltip)}>{exactText}</Tooltip>
    </TooltipTriggerStateContext.Provider>}
  </ThemePortal>
})

/**
 * Pointer-hover tooltip state. RAC's TooltipTrigger requires a focusable trigger, which would add a
 * Tab stop per timestamp; keyboard and screen-reader users get the same text as `aria-description`.
 */
const useHoverTooltip = (enabled: boolean, delay: number): { readonly state: TooltipTriggerState; readonly hoverProps: React.DOMAttributes<Element> } => {
  const [isOpen, setOpen] = React.useState(false)
  const timer = React.useRef<ReturnType<typeof setTimeout> | undefined>(undefined)
  const clear = () => { clearTimeout(timer.current); timer.current = undefined }
  React.useEffect(() => clear, [])
  const { hoverProps } = useHover({
    isDisabled: !enabled,
    onHoverStart: () => { clear(); timer.current = setTimeout(() => setOpen(true), delay) },
    onHoverEnd: () => { clear(); setOpen(false) },
  })
  const state: TooltipTriggerState = { isOpen, shouldSkipAnimation: false, open: () => setOpen(true), close: () => { clear(); setOpen(false) } }
  return { state, hoverProps }
}

const styles = stylex.create({
  time: { fontVariantNumeric: 'tabular-nums', outline: 'none' },
  // RAC supplies placement only after measuring the real mounted anchor.
  tooltip: { visibility: { default: 'hidden', ':is([data-placement])': 'visible' }, width: 'max-content' },
})
