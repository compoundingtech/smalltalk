import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { Button as AriaButton, type ButtonProps } from 'react-aria-components'
import { surfaceVars as surface, textVars as text, borderVars as border, accentVars as accent, geometryVars as g, radiusVars as r, spaceVars as s, typeVars as t } from '../composition-tokens.stylex'
import { Icon, type IconName } from './Icons'

export function Button({ children, variant = 'ghost', size = 'sm', ...props }: ButtonProps & { variant?: 'ghost' | 'outline'; size?: 'sm' | 'md' | 'lg' }) {
  return <AriaButton {...props} {...stylex.props(styles.button, styles[size], variant === 'outline' && styles.outline)}>{children}</AriaButton>
}
export function IconButton({ icon, label, ...props }: Omit<ButtonProps, 'children'> & { icon: IconName; label: string }) {
  return <Button {...props} aria-label={label}><Icon name={icon} /></Button>
}
const styles = stylex.create({
  button: { display: 'inline-flex', alignItems: 'center', justifyContent: 'center', gap: s.xs, paddingInline: s.sm, borderWidth: 0, borderRadius: r.control, backgroundColor: 'transparent', color: text.fgMuted, fontSize: t.metaSize, lineHeight: t.metaLeading, cursor: 'pointer', flexShrink: 0, ':hover': { backgroundColor: surface.rowHover, color: text.fg }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary, outlineOffset: g.hairline }, ':disabled': { opacity: 0.64, cursor: 'default' } },
  sm: { minWidth: g.controlSm, height: g.controlSm }, md: { minWidth: g.controlMd, height: g.controlMd }, lg: { minWidth: g.controlLg, height: g.controlLg },
  outline: { borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, backgroundColor: surface.controlFill, color: text.fg },
})
