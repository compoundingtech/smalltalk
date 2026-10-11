import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { UNSAFE_PortalProvider } from 'react-aria'
import { spaceVars as s } from '../composition-tokens.stylex'
/** Portal inside the themed surface: arbitrary token themes reach hover cards and menus without component overrides. */
export function ThemePortal({ children }: { children: React.ReactNode }) {
  const host = React.useRef<HTMLDivElement>(null)
  const getContainer = React.useCallback(() => host.current, [])
  return <UNSAFE_PortalProvider getContainer={getContainer}>{children}<div ref={host} {...stylex.props(styles.host)} /></UNSAFE_PortalProvider>
}
const styles = stylex.create({ host: { position: 'fixed', top: s.zero, left: s.zero, width: s.zero, height: s.zero, zIndex: 20 } })
