import * as stylex from '@stylexjs/stylex'
import * as React from 'react'

import { scale, tokens } from '../ui-compat/tokens.stylex.ts'
import { incrDebugRuntime, RenderProfiler } from '../telemetry/meters.tsx'

import type { UnobservedFeed } from '../data/FeedState.tsx'
import {
  useDataSource,
  useFeedInterest,
  useTerminal,
  useTerminalConnected,
} from '../data/react.tsx'
import type { NativeSubjectProps } from '../resources/react.tsx'
import { DomTerminal } from './DomTerminal.tsx'
import { unavailableTerminalHistory } from './historySource.ts'
import { defaultTerminalPalette } from './palette.ts'
import { TerminalHistory } from './TerminalHistory.tsx'
import { TerminalKeyboardBinding } from './TerminalKeyboardBinding.tsx'
import { TerminalPane } from './TerminalPane.tsx'
import { TerminalResize } from './TerminalResize.tsx'

/** Fixed copy for a terminal without an observed screen; transport details never reach the view. */
export const terminalUnobservedCopy = (feed: UnobservedFeed): string =>
  feed._tag === 'Waiting'
    ? 'Loading terminal…'
    : feed.reason === 'ungranted'
      ? 'This terminal is not available to this view.'
      : feed.reason === 'unsupported'
        ? 'This gateway does not provide terminals.'
        : 'The terminal could not be opened. Return to the thread and open it again.'

/** Native terminal observations retain their real grid and lifecycle instead of inventing an envelope. */
export const TerminalDetail = React.memo(
  ({ address, visibility }: NativeSubjectProps) => {
    incrDebugRuntime('Fn.terminal')
    const connected = useTerminalConnected(address.ref)
    const source = useDataSource()
    useFeedInterest({
      interest: source.terminalInterest?.(address.ref),
      visible: visibility === 'visible',
    })
    const terminal = useTerminal(address.ref)
    if (terminal._tag !== 'Observed')
      return (
        <p
          role="status"
          data-terminal-state={terminal._tag === 'Waiting' ? 'loading' : terminal.reason}
          {...stylex.props(styles.unobserved)}
        >
          {terminalUnobservedCopy(terminal)}
        </p>
      )
    const screen = terminal.value
    const resizeReason =
      terminal.freshness === 'stale' || connected === false
        ? 'The terminal is disconnected or stale. Resize is paused until a live screen returns.'
        : undefined
    const inputReason =
      visibility !== 'visible' || terminal.freshness === 'stale' || connected === false
        ? 'Input is paused while this terminal is hidden, disconnected or stale. Re-enter explicitly when a live screen returns.'
        : undefined
    return (
      <div {...stylex.props(styles.root)}>
        {terminal.freshness === 'stale' && (
          <p role="status" {...stylex.props(styles.stale)}>
            Terminal snapshot is stale.
          </p>
        )}
        <RenderProfiler id="terminal">
          <TerminalPane
            screen={screen}
            state={connected === false ? 'ended' : terminal.freshness !== 'live' ? 'stale' : visibility === 'visible' ? 'live' : 'idle'}
            controls={
              <TerminalResize
                key={screen.runtime_incarnation}
                screen={screen}
                {...(source.terminalResize === undefined ? {} : { port: source.terminalResize })}
                {...(resizeReason === undefined ? {} : { disabledReason: resizeReason })}
              />
            }
          >
            <TerminalHistory
              port={(source.terminalHistory ?? unavailableTerminalHistory)(
                screen.terminal_id,
                screen.runtime_incarnation,
              )}
              palette={defaultTerminalPalette}
              incarnation={screen.runtime_incarnation}
              alternateScreen={screen.modes.alternate_screen}
            />
            <DomTerminal screen={screen} palette={defaultTerminalPalette} />
            <TerminalKeyboardBinding
              terminalRef={screen.terminal_id}
              incarnation={screen.runtime_incarnation}
              modes={screen.modes}
              {...(source.terminalInput === undefined ? {} : { factory: source.terminalInput })}
              {...(inputReason === undefined ? {} : { unavailableReason: inputReason })}
            />
          </TerminalPane>
        </RenderProfiler>
      </div>
    )
  },
  (left, right) =>
    left.address.ref === right.address.ref &&
    left.address.presentation === right.address.presentation &&
    left.visibility === right.visibility,
)

const styles = stylex.create({
  root: {
    padding: scale.space2,
    display: 'flex',
    flexDirection: 'column',
    flexGrow: 1,
    minHeight: 0,
    minWidth: 0,
    overflow: 'auto',
  },
  stale: { padding: scale.space2, color: tokens['--ds-gray-900'], fontSize: '0.8125rem' },
  unobserved: { margin: 'auto', padding: scale.space4, color: tokens['--ds-gray-900'], fontSize: '0.8125rem' },
})
