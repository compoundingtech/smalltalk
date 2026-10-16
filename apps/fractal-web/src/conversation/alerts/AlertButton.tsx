import * as stylex from '@stylexjs/stylex'
import * as React from 'react'
import * as Aria from 'react-aria-components'

import { colorVars as c, typeVars as t } from '../../../../../packages/fractal-ui/src/assistant-ui/composition-tokens.stylex.ts'

/**
 * An answer button in the alert strip, styled with the composition tokens. The `ui-compat`
 * Button wraps the kit's Tailwind-classed Button, and Fractal web loads no Tailwind stylesheet,
 * so it rendered as bare run-together text in the live app.
 */
export const AlertButton = ({
  tone = 'secondary',
  ...props
}: Omit<Aria.ButtonProps, 'className' | 'style'> & { readonly tone?: 'primary' | 'secondary' | 'danger' }): React.ReactNode => (
  <Aria.Button {...props} {...stylex.props(styles.button, tone === 'primary' ? styles.primary : tone === 'danger' ? styles.danger : styles.secondary)} />
)

const styles = stylex.create({
  button: {
    display: 'inline-flex',
    alignItems: 'center',
    minHeight: 28,
    paddingInline: 10,
    borderRadius: 6,
    borderWidth: 1,
    borderStyle: 'solid',
    fontFamily: t.fontSans,
    fontSize: t.metaSize,
    fontWeight: 500,
    lineHeight: 1.2,
    cursor: 'pointer',
    ':focus-visible': { outline: `2px solid ${c.primary}`, outlineOffset: 1 },
    ':disabled': { opacity: 0.45, cursor: 'default' },
  },
  primary: { backgroundColor: c.primary, borderColor: c.primary, color: c.onPrimary, ':hover': { backgroundColor: c.primaryHover } },
  secondary: { backgroundColor: c.raised, borderColor: c.borderStrong, color: c.fg, ':hover': { backgroundColor: c.rowHover } },
  danger: { backgroundColor: 'transparent', borderColor: c.dangerFg, color: c.dangerFg, ':hover': { backgroundColor: c.rowHover } },
})
