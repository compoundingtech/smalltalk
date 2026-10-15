// Chrome controls: thread header, sidebar header, toolbar and footer. Explicit-fact presentation only.
import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { Button, Focusable, Pressable } from 'react-aria-components'
import { accentVars as accent, geometryVars as g, motionVars as m, radiusVars as r, surfaceVars as surface, textVars as ink } from '../composition-tokens.stylex'
import { Icon, type IconName } from './Icons'
import { Tooltip } from './Controls'

export interface ChromeControlProps {
  readonly icon: IconName
  /** Accessible name and tooltip text: "Toggle right panel", "Actions for fractal-web". */
  readonly label: string
  /** Toggle state; omitted for plain actions. A pressed toggle keeps the hover fill. */
  readonly pressed?: boolean
  /** The fill without toggle semantics: a menu trigger whose filter or sort is in effect. The label says what is set. */
  readonly active?: boolean
  /**
   * Why the control cannot run. It stays in the tab order as an `aria-disabled` button that never presses,
   * is described by the reason, and shows "label · reason" in the kit tooltip on hover and keyboard focus.
   */
  readonly disabledReason?: string
  /** Briefly unavailable, e.g. while its own action is in flight. */
  readonly isDisabled?: boolean
  readonly onPress?: () => void
  readonly tooltipPlacement?: 'top' | 'bottom'
}

/**
 * One quiet 28px icon button for the chrome: borderless, filled on hover, named by the kit tooltip.
 * Inside a React Aria trigger (MenuTrigger, DialogTrigger) it is the trigger's button.
 */
export function ChromeControl({ icon, label, pressed, active = false, disabledReason, isDisabled = false, onPress, tooltipPlacement = 'bottom' }: ChromeControlProps) {
  const reasonId = React.useId()
  if (disabledReason !== undefined) {
    // A natively disabled button leaves the tab order and receives no pointer events, so its reason would be
    // unreachable. The disabled Pressable consumes a surrounding trigger's press (DialogTrigger, MenuTrigger) and
    // fires nothing; Focusable hands hover and focus to the kit tooltip. A MenuTrigger also puts its own key
    // handler on the button, which opens on Enter, Space and the arrows unless the event is default-prevented,
    // so the capture phase prevents those keys before any trigger sees them. Tab and Escape still pass.
    return <>
      <Tooltip label={`${label} · ${disabledReason}`} placement={tooltipPlacement}>
        <Focusable>
          <Pressable isDisabled>
            <button
              onKeyDownCapture={event => { if (triggerKeys[event.key] === true) event.preventDefault() }}
              type="button"
              aria-label={label}
              aria-disabled="true"
              aria-describedby={reasonId}
              {...(pressed === undefined ? {} : { 'aria-pressed': pressed })}
              {...stylex.props(styles.control, styles.disabled)}
            >
              <Icon name={icon} />
            </button>
          </Pressable>
        </Focusable>
      </Tooltip>
      <span id={reasonId} hidden>{disabledReason}</span>
    </>
  }
  return <Tooltip label={label} placement={tooltipPlacement}>
    <Button
      aria-label={label}
      {...(pressed === undefined ? {} : { 'aria-pressed': pressed })}
      isDisabled={isDisabled}
      {...(onPress === undefined ? {} : { onPress })}
      {...stylex.props(styles.control, (pressed === true || active) && styles.pressed)}
    >
      <Icon name={icon} />
    </Button>
  </Tooltip>
}

/** Keys a React Aria trigger acts on; a control with a reason swallows them. */
const triggerKeys: Record<string, true> = { Enter: true, ' ': true, ArrowDown: true, ArrowUp: true }

const styles = stylex.create({
  control: {
    display: 'inline-flex', alignItems: 'center', justifyContent: 'center', flexShrink: 0, boxSizing: 'border-box',
    width: g.controlMd, height: g.controlMd, padding: 0,
    borderWidth: 0, borderRadius: r.control, backgroundColor: 'transparent', color: ink.fgMuted,
    cursor: 'pointer',
    transitionProperty: 'background-color, color', transitionDuration: m.fast, transitionTimingFunction: m.ease,
    ':hover': { backgroundColor: surface.rowHover, color: ink.fg },
    ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary, outlineOffset: g.focusOffset },
    ':disabled': { opacity: 0.64, cursor: 'default', backgroundColor: 'transparent', color: ink.fgMuted },
  },
  pressed: { backgroundColor: surface.rowActive, color: ink.fg, ':hover': { backgroundColor: surface.rowActive, color: ink.fg } },
  // The aria-disabled twin of `:disabled`: dimmed and inert to hover, but it keeps the focus ring.
  disabled: { opacity: 0.64, cursor: 'default', ':hover': { backgroundColor: 'transparent', color: ink.fgMuted } },
})
