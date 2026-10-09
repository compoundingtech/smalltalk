import type * as React from 'react'
import type { TerminalColor, TerminalScreen } from '../../../../../clients/typescript/st3-client/Models.generated'
export type { TerminalScreen }
export type TerminalColorInput = TerminalColor
export interface TerminalPalette { readonly resolve: (color: TerminalColorInput) => string; readonly foreground: string; readonly background: string; readonly selection: string; readonly cursor: string; readonly error: string }
export interface TerminalFont { readonly family: string; readonly sizePx: number; readonly lineHeightPx: number; readonly advanceEm?: number }
export type TerminalConnection = { readonly state: 'live' } | { readonly state: 'connecting' } | { readonly state: 'reconnecting'; readonly detail?: string } | { readonly state: 'ended' | 'unavailable'; readonly reason: string }
export interface TerminalSurfaceHandle { readonly focus: () => void; readonly copySelection: () => string | undefined; readonly clearSelection: () => void; readonly measure: () => { readonly cols: number; readonly rows: number } | undefined }
export interface TerminalSurfaceProps {
  readonly screen: TerminalScreen | null; readonly connection: TerminalConnection; readonly readOnly: boolean; readonly palette: TerminalPalette
  readonly font?: TerminalFont; readonly scrollbackLines?: number; readonly focusRing?: boolean; readonly readOnlyReason?: string
  readonly onInput?: (data: string) => void; readonly onPaste?: (text: string) => void; readonly onResize?: (size: { readonly cols: number; readonly rows: number }) => void
  readonly onCopy?: (text: string) => void; readonly onFocusChange?: (focused: boolean) => void; readonly onBell?: () => void
  readonly onRecover?: () => void; readonly recoveryLabel?: string; readonly handleRef?: React.Ref<TerminalSurfaceHandle>; readonly label?: string
}
/** One terminal per agent. Closing detaches the view, not the process. */
export interface TerminalDrawerProps extends Omit<TerminalSurfaceProps, 'handleRef'> {
  readonly open: boolean; readonly height: number; readonly onHeight: (px: number) => void; readonly onToggle?: () => void
  readonly onReset?: () => void; readonly min?: number; readonly max?: number; readonly onDetach?: () => void
  readonly onKill?: () => void; readonly onAdd?: () => void
}
