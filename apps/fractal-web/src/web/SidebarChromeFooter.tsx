import * as React from 'react'
import * as Aria from 'react-aria-components'
import * as stylex from '@stylexjs/stylex'
import { Icon } from '../../../../packages/fractal-ui/src/assistant-ui/composition/Icons.tsx'
import { colorVars as c, typeVars as t, spaceVars as s } from '../../../../packages/fractal-ui/src/assistant-ui/composition-tokens.stylex.ts'

/** View controls only: the workspace owns the single offline notice and reconnect action. */
export const SidebarChromeFooter = ({ onToggleScheme, children }: {
  readonly onToggleScheme: () => void
  readonly children?: React.ReactNode
}) => <footer {...stylex.props(styles.footer)}>
  <Aria.Button onPress={onToggleScheme} aria-label="Toggle color scheme" {...stylex.props(styles.iconButton)}><Icon name="swap" /></Aria.Button>
  {children}
</footer>

/** Render only after the gateway has a live observation and the network is online. */
export const SidebarConnectedStatus = () => <span role="status" {...stylex.props(styles.connection)}>
  <span aria-hidden="true" data-sidebar-connection-dot="" {...stylex.props(styles.dot)} />
  Connected
</span>

const styles = stylex.create({
  footer: { height: 40, display: 'flex', alignItems: 'center', gap: s.md, paddingInline: s.md, flexShrink: 0 },
  iconButton: { width: 28, height: 28, display: 'inline-flex', justifyContent: 'center', alignItems: 'center', backgroundColor: 'transparent', color: c.fgMuted, borderWidth: 0, borderRadius: 6, cursor: 'pointer', ':focus-visible': { outline: `2px solid ${c.primary}`, outlineOffset: 2 } },
  connection: { marginLeft: 'auto', display: 'inline-flex', alignItems: 'center', gap: s.md, fontSize: t.denseSize, color: c.fgMuted },
  dot: { width: 8, height: 8, flexShrink: 0, borderRadius: '50%', backgroundColor: c.done },
})
