import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { Button as AriaButton, Checkbox as AriaCheckbox, Menu as AriaMenu, MenuItem as AriaMenuItem, Popover as AriaPopover, Tooltip as AriaTooltip, TooltipTrigger, type ButtonProps, type CheckboxProps as AriaCheckboxProps, type MenuProps as AriaMenuProps, type MenuItemProps as AriaMenuItemProps, type PopoverProps as AriaPopoverProps, type TooltipProps as AriaTooltipProps } from 'react-aria-components'
import { surfaceVars as surface, textVars as text, borderVars as border, accentVars as accent, interactionVars as interaction, geometryVars as g, geometryNumbers, radiusVars as r, spaceVars as s, typeVars as t, elevationVars as elevation, motionVars as motion } from '../composition-tokens.stylex'
import { Icon, type IconName } from './Icons'
import { ThemePortal } from '../taste/ThemePortal'

export function Button({ children, variant = 'ghost', size = 'sm', ...props }: ButtonProps & { variant?: 'ghost' | 'outline'; size?: 'sm' | 'md' | 'lg' }) {
  return <AriaButton {...props} {...stylex.props(styles.button, styles[size], variant === 'outline' && styles.outline)}>{children}</AriaButton>
}
export function IconButton({ icon, label, ...props }: Omit<ButtonProps, 'children'> & { icon: IconName; label: string }) {
  return <Button {...props} aria-label={label}><Icon name={icon} /></Button>
}
export { MenuTrigger } from 'react-aria-components'

export type MenuProps<T> = Omit<AriaMenuProps<T>, 'className' | 'style'> & { style?: stylex.StyleXStyles }
export function Menu<T extends object>({ style, ...props }: MenuProps<T>) {
  return <AriaMenu {...props} {...stylex.props(styles.menu, style)} />
}
export type MenuItemProps<T> = Omit<AriaMenuItemProps<T>, 'className' | 'style'> & { leading?: React.ReactNode; shortcut?: string; style?: stylex.StyleXStyles }
/** RAC owns checkbox/radio semantics. Selectable items and items with a leading glyph reserve the 16px slot, so selection never shifts the label. */
export function MenuItem<T extends object>({ children, leading, shortcut, style, ...props }: MenuItemProps<T>) {
  return <AriaMenuItem {...props} {...stylex.props(styles.menuItem, style)}>{state => <>
    {(state.selectionMode !== 'none' || leading !== undefined) && <span aria-hidden="true" {...stylex.props(styles.leading)}>{state.isSelected ? <CheckMark /> : leading}</span>}
    <span {...stylex.props(styles.label)}>{typeof children === 'function' ? children(state) : children}</span>
    {/* Muted ink drops below AA on the dark wash, so an active row lifts its shortcut to soft ink. */}
    {shortcut && <kbd {...stylex.props(styles.shortcut, (state.isHovered || state.isFocused) && styles.shortcutActive)}>{shortcut}</kbd>}
  </>}</AriaMenuItem>
}
export type PopoverProps = Omit<AriaPopoverProps, 'className' | 'style'> & { style?: stylex.StyleXStyles }
/** The host remains under the caller's theme while RAC owns dismissal and focus restoration. */
export function Popover({ style, offset = geometryNumbers.contextOffset, ...props }: PopoverProps) {
  return <ThemePortal><AriaPopover {...props} offset={offset} {...stylex.props(styles.popover, styles.surfaceMotion, style)} /></ThemePortal>
}
export type CheckboxProps = Omit<AriaCheckboxProps, 'className' | 'style'> & { style?: stylex.StyleXStyles }
export function Checkbox({ children, style, ...props }: CheckboxProps) {
  return <AriaCheckbox {...props} {...stylex.props(styles.checkbox, style)}>{state => <>
    <span aria-hidden="true" {...stylex.props(styles.checkboxMark, (state.isSelected || state.isIndeterminate) && styles.checkboxOn)}>
      {(state.isSelected || state.isIndeterminate) && <CheckMark indeterminate={state.isIndeterminate} />}
    </span>
    {typeof children === 'function' ? children(state) : children}
  </>}</AriaCheckbox>
}
function CheckMark({ indeterminate = false }: { indeterminate?: boolean }) {
  return <svg aria-hidden="true" viewBox="0 0 14 14" fill="none" {...stylex.props(styles.check)}><path d={indeterminate ? 'M3 7h8' : 'm3 7 2.5 2.5L11 4'} stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" /></svg>
}
/** Quiet time after scrolling before the keyboard-focused tooltip reopens. */
const scrollSettleMs = 100
const tooltipOpenEvent = 'fractal-kit-tooltip-open'
export type TooltipProps = { label: string; shortcut?: string; placement?: 'top' | 'bottom'; delay?: number; isOpen?: boolean; onOpenChange?: (isOpen: boolean) => void; children: React.ReactNode; style?: stylex.StyleXStyles }
/** `isOpen` lets a trigger hold its tooltip open to acknowledge an action, e.g. "Copied". */
export function Tooltip({ label, shortcut, placement = 'bottom', delay = 350, isOpen: controlledOpen, onOpenChange, children, style }: TooltipProps) {
  const id = React.useId()
  const scope = React.useRef<HTMLSpanElement>(null)
  const lastScroll = React.useRef(Number.NEGATIVE_INFINITY)
  const reopenRequest = React.useRef(0)
  const [localOpen, setLocalOpen] = React.useState(false)
  const isOpen = controlledOpen ?? localOpen
  const [settling, setSettling] = React.useState(false)
  const setOpen = (open: boolean) => { setLocalOpen(open); onOpenChange?.(open) }
  const keyboardFocused = () => {
    const active = scope.current?.ownerDocument.activeElement
    return active != null && scope.current!.contains(active) && active.matches(':focus-visible')
  }
  React.useEffect(() => {
    if (!isOpen && !settling) return
    let timer: number | undefined
    const settle = () => {
      window.clearTimeout(timer)
      timer = window.setTimeout(() => { setSettling(false); if (keyboardFocused()) setOpen(true) }, scrollSettleMs)
    }
    const onScroll = () => {
      lastScroll.current = performance.now()
      if (settling) settle()
    }
    window.addEventListener('scroll', onScroll, true)
    if (settling) settle()
    return () => { window.clearTimeout(timer); window.removeEventListener('scroll', onScroll, true) }
  }, [isOpen, settling])
  React.useEffect(() => {
    if (!isOpen && !settling) return
    const onOtherOpen = (event: Event) => { if ((event as CustomEvent<string>).detail !== id) { reopenRequest.current++; lastScroll.current = Number.NEGATIVE_INFINITY; setSettling(false); setOpen(false) } }
    window.addEventListener(tooltipOpenEvent, onOtherOpen)
    if (isOpen) window.dispatchEvent(new CustomEvent(tooltipOpenEvent, { detail: id }))
    return () => window.removeEventListener(tooltipOpenEvent, onOtherOpen)
  }, [isOpen, settling, id])
  const changeOpen = (open: boolean) => {
    setOpen(open)
    if (open) return
    const closedAt = performance.now()
    const request = ++reopenRequest.current
    window.setTimeout(() => { if (request === reopenRequest.current && Math.abs(closedAt - lastScroll.current) < scrollSettleMs && keyboardFocused()) setSettling(true) })
  }
  return <ThemePortal><span ref={scope} onKeyDownCapture={event => { if (event.key === 'Escape') { reopenRequest.current++; lastScroll.current = Number.NEGATIVE_INFINITY; setSettling(false); setOpen(false) } }} {...stylex.props(styles.tooltipScope)}><TooltipTrigger delay={delay} closeDelay={0} isOpen={isOpen} onOpenChange={changeOpen}>{children}<TooltipSurface placement={placement} style={style}><span>{label}</span>{shortcut && <>{' '}<kbd {...stylex.props(styles.shortcut, styles.tooltipShortcut)}>{shortcut}</kbd></>}</TooltipSurface></TooltipTrigger></span></ThemePortal>
}
export type TooltipSurfaceProps = Omit<AriaTooltipProps, 'className' | 'style'> & { style?: stylex.StyleXStyles }
/** Use inside an existing TooltipTrigger and ThemePortal when its trigger is not a kit Button. */
export const TooltipSurface = React.forwardRef<HTMLDivElement, TooltipSurfaceProps>(function TooltipSurface({ style, offset = geometryNumbers.tooltipOffset, ...props }, ref) {
  return <AriaTooltip {...props} ref={ref} offset={offset} {...stylex.props(tooltipStyles.tooltip, style)} />
})
const enterSurface = stylex.keyframes({ from: { opacity: 0, transform: motion.surfaceTransform }, to: { opacity: 1, transform: 'scale(1)' } })
const exitSurface = stylex.keyframes({ from: { opacity: 1, transform: 'scale(1)' }, to: { opacity: 0, transform: motion.surfaceTransform } })
const styles = stylex.create({
  button: { display: 'inline-flex', alignItems: 'center', justifyContent: 'center', gap: s.xs, paddingInline: s.sm, borderWidth: 0, borderRadius: r.control, backgroundColor: 'transparent', color: text.fgMuted, fontSize: t.metaSize, lineHeight: t.metaLeading, cursor: 'pointer', flexShrink: 0, ':hover': { backgroundColor: surface.rowHover, color: text.fg }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: interaction.focusRing, outlineOffset: g.hairline }, ':disabled': { opacity: 0.64, cursor: 'default' } },
  sm: { minWidth: g.controlSm, height: g.controlSm }, md: { minWidth: g.controlMd, height: g.controlMd }, lg: { minWidth: g.controlLg, height: g.controlLg },
  outline: { borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, backgroundColor: surface.controlFill, color: text.fg },
  // The portal host is a 0×0 fixed box, so shrink-to-fit would collapse the surface to its min-content width; size it to its rows up to menuMax.
  popover: { zIndex: 50, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.border, borderRadius: r.md, backgroundColor: surface.raised, color: text.fg, padding: s.xs, boxShadow: elevation.popover, width: 'max-content', maxWidth: g.menuMax },
  menu: { minWidth: g.menuMin, outline: 'none', fontFamily: t.fontSans },
  menuItem: { display: 'flex', alignItems: 'center', gap: s.md, minHeight: g.menuRow, boxSizing: 'border-box', paddingInline: s.md, borderRadius: r.sm, color: text.fg, fontSize: t.uiSize, lineHeight: t.uiLeading, cursor: 'default', outline: 'none', ':is([data-hovered], [data-focused])': { backgroundColor: interaction.wash }, ':is([data-focus-visible])': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: interaction.focusRing, outlineOffset: `calc(${g.focusRing} * -1)` }, ':is([data-disabled])': { color: text.fgMuted, opacity: 0.64 } },
  leading: { width: g.icon, height: g.icon, display: 'inline-flex', alignItems: 'center', justifyContent: 'center', flexShrink: 0 },
  label: { flexGrow: 1, minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' },
  check: { width: g.checkbox, height: g.checkbox, flexShrink: 0 },
  checkbox: { display: 'inline-flex', alignItems: 'center', gap: s.md, minHeight: g.controlSm, color: text.fg, fontFamily: t.fontSans, fontSize: t.uiSize, lineHeight: t.uiLeading, cursor: 'pointer', borderRadius: r.keyboard, ':is([data-focus-visible])': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: interaction.focusRing, outlineOffset: g.focusOffset }, ':is([data-disabled])': { opacity: 0.64, cursor: 'default' } },
  checkboxMark: { width: g.checkbox, height: g.checkbox, boxSizing: 'border-box', flexShrink: 0, display: 'inline-flex', alignItems: 'center', justifyContent: 'center', borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.keyboard, backgroundColor: surface.raised },
  checkboxOn: { backgroundColor: accent.primary, borderColor: accent.primary, color: accent.onPrimary },
  tooltipScope: { display: 'contents' },
  shortcut: { flexShrink: 0, color: text.fgMuted, fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading },
  tooltipShortcut: { color: 'inherit' },
  shortcutActive: { color: text.fgSoft },
  surfaceMotion: { transformOrigin: 'center', animationDuration: motion.fast, animationTimingFunction: motion.ease, ':is([data-entering])': { animationName: enterSurface }, ':is([data-exiting])': { animationName: exitSurface }, '@media (prefers-reduced-motion: reduce)': { animationName: 'none', animationDuration: '0s', transform: 'none', ':is([data-entering])': { animationName: 'none' }, ':is([data-exiting])': { animationName: 'none' } } },
})
/** Shared tooltip surface; portal ownership and trigger behavior stay with each caller. */
export const tooltipStyles = stylex.create({
  tooltip: { zIndex: 50, borderRadius: r.control, backgroundColor: text.fg, paddingInline: s.row, paddingBlock: s.sm, fontSize: t.denseSize, fontFamily: t.fontSans, color: surface.raised, boxShadow: elevation.popover, width: 'max-content', maxWidth: g.tooltipMax, whiteSpace: 'normal', overflowWrap: 'break-word', textAlign: 'start' },
})
