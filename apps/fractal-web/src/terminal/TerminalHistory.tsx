import { useAtomValue } from '@effect/atom-react'
import * as stylex from '@stylexjs/stylex'
import * as React from 'react'
import { Button } from 'react-aria-components'

import { tokens } from '../ui-compat/tokens.stylex.ts'

import { TERMINAL_FONT_SIZE, TERMINAL_LINE_HEIGHT, TerminalLineView } from './DomTerminal.tsx'
import type { HistoryState } from './history.ts'
import type { TerminalHistoryPort } from './historySource.ts'
import type { TerminalPalette } from './palette.ts'
import { TERMINAL_FONT_FAMILY, terminalFontsReady } from './terminalFonts.ts'

const styles = stylex.create({
  root: { display: 'flex', flexDirection: 'column', alignItems: 'flex-start', minWidth: 0 },
  controls: {
    display: 'flex',
    alignItems: 'center',
    gap: 8,
    padding: '6px 0',
    fontSize: 12,
    color: tokens['--ds-gray-900'],
  },
  button: {
    padding: '3px 8px',
    borderRadius: 4,
    borderWidth: 1,
    borderStyle: 'solid',
    borderColor: tokens['--ds-gray-500'],
    color: tokens['--ds-gray-1000'],
    backgroundColor: tokens['--ds-background-100'],
    cursor: 'pointer',
    outline: { default: 'none', ':focus-visible': `2px solid ${tokens['--ds-focus-color']}` },
  },
  note: { margin: '6px 0', fontSize: 12, color: tokens['--ds-gray-900'] },
  rows: {
    fontVariantLigatures: 'none',
    fontKerning: 'none',
    fontFeatureSettings: '"liga" 0, "calt" 0',
  },
})

/** Props for rendering a retained history projection. */
export interface TerminalHistoryViewProps {
  readonly state: HistoryState
  readonly palette: TerminalPalette
  /** Both guards come from the current authoritative screen, not cached history. */
  readonly incarnation: string
  readonly alternateScreen: boolean
  readonly onLoadOlder?: () => void
  readonly onRefresh?: () => void
}

/** Place before DomTerminal in the same scroll viewport; native copy/find see the same row markup. */
export const TerminalHistoryView = ({
  state,
  palette,
  incarnation,
  alternateScreen,
  onLoadOlder,
  onRefresh,
}: TerminalHistoryViewProps) => {
  if (alternateScreen)
    return (
      <p role="status" {...stylex.props(styles.note)}>
        Main-buffer history is hidden while the alternate screen is active.
      </p>
    )
  if (state._tag === 'Loading')
    return (
      <p role="status" {...stylex.props(styles.note)}>
        Loading terminal history…
      </p>
    )
  if (state._tag === 'Ready' && state.window.incarnation !== incarnation)
    return (
      <p role="status" {...stylex.props(styles.note)}>
        The terminal session changed. History from the previous session is not shown.
      </p>
    )
  const issue = state.issue
  const window = state._tag === 'Ready' ? state.window : undefined
  const busy = state._tag === 'Ready' && state.activity === 'loading'
  const unavailable = issue?._tag === 'Unavailable' || issue?._tag === 'IncarnationChanged'
  return (
    <section aria-label="Terminal history" {...stylex.props(styles.root)}>
      <div {...stylex.props(styles.controls)}>
        {window !== undefined && (
          <span>
            {window.lines.length} loaded · {window.retainedRows} retained in owner memory
          </span>
        )}
        {window?.nextBefore !== undefined && (
          <Button
            {...stylex.props(styles.button)}
            isDisabled={busy || issue !== undefined || onLoadOlder === undefined}
            {...(onLoadOlder === undefined ? {} : { onPress: onLoadOlder })}
          >
            {busy ? 'Loading…' : 'Load older rows'}
          </Button>
        )}
        {!unavailable && onRefresh !== undefined && (
          <Button {...stylex.props(styles.button)} isDisabled={busy} onPress={onRefresh}>
            Refresh history
          </Button>
        )}
      </div>
      {issue !== undefined && (
        <p role={issue._tag === 'Failed' ? 'alert' : 'status'} {...stylex.props(styles.note)}>
          {issue.detail}
        </p>
      )}
      {window !== undefined && window.lines.length === 0 && (
        <p role="status" {...stylex.props(styles.note)}>
          No retained main-buffer history. The current screen follows below.
        </p>
      )}
      {window !== undefined && (
        <React.Suspense fallback={<p role="status">Loading terminal fonts…</p>}>
          <HistoryRows window={window} palette={palette} />
        </React.Suspense>
      )}
    </section>
  )
}

/** Bind a retained-history port to the history view for one incarnation. */
export const TerminalHistory = ({
  port,
  ...props
}: Omit<TerminalHistoryViewProps, 'state' | 'onLoadOlder' | 'onRefresh'> & {
  readonly port: TerminalHistoryPort
}) => (
  <TerminalHistoryView
    {...props}
    state={useAtomValue(port.state)}
    {...(port.loadOlder === undefined ? {} : { onLoadOlder: port.loadOlder })}
    {...(port.refresh === undefined ? {} : { onRefresh: port.refresh })}
  />
)

const HistoryRows = ({
  window,
  palette,
}: {
  readonly window: Extract<HistoryState, { _tag: 'Ready' }>['window']
  readonly palette: TerminalPalette
}) => {
  const fonts = React.use(terminalFontsReady())
  if (fonts._tag === 'Failed')
    return <p role="alert">Terminal fonts could not be loaded. Reload this page to try again.</p>
  return (
    <div
      {...stylex.props(styles.rows)}
      style={{
        fontFamily: TERMINAL_FONT_FAMILY,
        fontSize: TERMINAL_FONT_SIZE,
        lineHeight: `${TERMINAL_LINE_HEIGHT}px`,
        width: `${window.columns}ch`,
      }}
    >
      {window.lines.map((line, index) => (
        <TerminalLineView key={`${window.lines.length - index}`} line={line} palette={palette} />
      ))}
    </div>
  )
}
