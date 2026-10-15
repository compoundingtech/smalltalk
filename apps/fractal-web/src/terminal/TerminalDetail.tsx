import type { PtySize, PtyTarget } from '@st3/sdk/effect'
import * as stylex from '@stylexjs/stylex'
import * as React from 'react'

import { Badge, type BadgeVariant, Note } from '../ui-compat/components.tsx'
import { scale, tokens } from '../ui-compat/tokens.stylex.ts'
import { incrDebugRuntime, RenderProfiler } from '../telemetry/meters.tsx'

import type { UnobservedFeed } from '../data/FeedState.tsx'
import { useDataSource, useFeedInterest, useGrants, useTerminal, useTerminalConnected } from '../data/react.tsx'
import type { NativeSubjectProps } from '../resources/react.tsx'
import { DomTerminal } from './DomTerminal.tsx'
import { unavailableTerminalHistory } from './historySource.ts'
import { defaultTerminalPalette } from './palette.ts'
import { rawTyping, TerminalSurface, useRawTerminal, type TerminalTyping } from './RawTerminal.tsx'
import {
  rawModeFor,
  terminalChip,
  terminalInputPath,
  terminalPresentation,
  type TerminalChip,
  type TerminalInputPath,
} from './rawTerminalState.ts'
import { TerminalHistory } from './TerminalHistory.tsx'
import { TerminalPane, type PaneState } from './TerminalPane.tsx'
import { TerminalResize } from './TerminalResize.tsx'
import { useGatewayTyping, useStandaloneEncoder } from './terminalTyping.ts'

/** A terminal source publishes a human reason; a blank or placeholder detail falls back to fixed copy. */
export const terminalUnobservedCopy = (feed: UnobservedFeed): string => {
  if (feed._tag === 'Waiting') return 'Loading terminal…'
  const reason = feed.detail.trim()
  if (reason !== '' && !/^(undefined|unknown|null)$/i.test(reason)) return reason
  return feed.reason === 'ungranted'
    ? 'This terminal is not available to this view.'
    : feed.reason === 'unsupported'
      ? 'This gateway does not provide terminals.'
      : 'The terminal could not be opened.'
}

const chipVariant: Record<TerminalChip['tone'], BadgeVariant> = {
  live: 'green-subtle',
  pending: 'amber-subtle',
  readonly: 'gray-subtle',
  ended: 'red-subtle',
}

/** The terminal identity a raw session attaches to, pinned to the address it was observed under. */
interface PinnedTarget {
  readonly ref: string
  readonly target: PtyTarget
  readonly size: PtySize
  readonly title: string
  /** A raw ATTACH to this incarnation fell back; typing moved to the gateway and the stream to PEEK. */
  readonly attachLost: boolean
}

/**
 * One terminal surface with one input decision. The screen comes from the raw PTY stream (ATTACH,
 * or view-only PEEK) and falls back to the projected screen when the stream is refused or keeps
 * failing. Typing rides a raw ATTACH when this device holds `terminal.control`; otherwise a device
 * holding `terminal.input` types through the gateway, and the shared size changes only through the
 * explicit resize control. The projected feed is followed unless a live ATTACH owns the terminal.
 */
export const TerminalDetail = React.memo(
  ({ address, visibility }: NativeSubjectProps) => {
    incrDebugRuntime('Fn.terminal')
    const connected = useTerminalConnected(address.ref)
    const source = useDataSource()
    const grants = useGrants()
    const terminal = useTerminal(address.ref)
    const visible = visibility === 'visible'
    const observed = terminal._tag === 'Observed' ? terminal.value : undefined
    const [pinned, setPinned] = React.useState<PinnedTarget | undefined>(undefined)
    const retarget =
      observed !== undefined &&
      (pinned?.ref !== address.ref ||
        pinned.target.terminalId !== observed.terminal_id ||
        pinned.target.incarnation !== observed.runtime_incarnation)
    if (retarget)
      setPinned({
        ref: address.ref,
        target: { terminalId: observed.terminal_id, incarnation: observed.runtime_incarnation },
        size: { cols: observed.columns, rows: observed.rows },
        title: observed.title,
        attachLost: false,
      })
    const current = pinned?.ref === address.ref ? pinned : undefined
    const decided = terminalInputPath({
      raw: source.terminalRaw !== undefined,
      gateway: source.terminalInput !== undefined,
      grants,
      attachLost: current?.attachLost === true,
    })
    const mode = rawModeFor(decided)
    const [typing, setTyping] = React.useState(false)
    const raw = useRawTerminal({
      port: source.terminalRaw,
      target: current?.target,
      mode,
      active: visible,
      size: current?.size ?? { cols: 80, rows: 24 },
      title: current?.title ?? 'Terminal',
    })
    const presentation = terminalPresentation(raw.preparation)
    // An ATTACH that cannot be used hands typing to the gateway and reopens the stream as PEEK.
    if (!retarget && mode === 'attach' && presentation._tag === 'Projected' && current !== undefined)
      setPinned({ ...current, attachLost: true })
    const attachLive =
      mode === 'attach' && presentation._tag === 'Raw' && raw.preparation._tag === 'Session' && raw.preparation.state._tag === 'Live'
    useFeedInterest({
      interest: source.terminalInterest?.(address.ref),
      // Gateway input and explicit resize fence on the projected screen; a stale fence or a lost
      // stream re-resolves the terminal through it as well.
      visible: visible && !attachLive,
    })
    const projectedLive = terminal._tag === 'Observed' && terminal.freshness === 'live' && connected !== false
    const gateway = useGatewayTyping({
      factory: source.terminalInput,
      subject: address.ref,
      terminalRef: current?.target.terminalId ?? '',
      incarnation: current?.target.incarnation ?? '',
      enabled: decided._tag === 'Gateway' && visible && projectedLive && current !== undefined,
    })
    const gatewayLive = decided._tag === 'Gateway' && visible && projectedLive
    const encoder = useStandaloneEncoder(decided._tag === 'Gateway' && presentation._tag === 'Projected')
    // Without an emulator the projected pane needs the standalone encoder before it can type.
    const input: TerminalInputPath =
      decided._tag === 'Gateway' && presentation._tag === 'Projected' && encoder._tag === 'Failed'
        ? { _tag: 'ReadOnly', reason: encoder.message }
        : decided
    // After a page hide the focused field still waits for a click, so the chip must not say Typing.
    const chip = terminalChip({ raw: raw.preparation, presentation, input, typing: typing && !(input._tag === 'Gateway' && gateway.disarmed) })
    const controls = (
      <>
        <Badge variant={chipVariant[chip.tone]} size="sm" role="status" data-terminal-mode={presentation._tag} data-terminal-input-path={input._tag}>
          {chip.label}
        </Badge>
        {input._tag !== 'Attach' && terminal._tag === 'Observed' && (
          <TerminalResize
            key={terminal.value.runtime_incarnation}
            screen={terminal.value}
            {...(source.terminalResize === undefined ? {} : { port: source.terminalResize })}
            {...(terminal.freshness === 'stale' || connected === false
              ? { disabledReason: 'The terminal is disconnected or stale. Resize is paused until a live screen returns.' }
              : {})}
          />
        )}
      </>
    )
    const reasons = [
      presentation._tag === 'Projected' ? presentation.reason : undefined,
      input._tag === 'ReadOnly' ? input.reason : undefined,
    ].filter((reason) => reason !== undefined)
    const banner = reasons.length === 0 ? {} : { banner: <Note variant="warning" size="sm">{reasons.join(' ')}</Note> }
    const rawScreen = raw.snapshot?.screen
    if (presentation._tag === 'Raw' && raw.handle !== undefined && rawScreen !== undefined && raw.preparation._tag === 'Session') {
      const handle = raw.handle
      const state = raw.preparation.state
      const paneState: PaneState =
        state._tag === 'Live'
          ? visible
            ? 'live'
            : 'idle'
          : state._tag === 'Connecting' && state.attempt === 1
            ? 'connecting'
            : state._tag === 'Exited' || state._tag === 'Closed'
              ? 'ended'
              : 'stale'
      const sink: TerminalTyping | undefined =
        input._tag === 'Attach'
          ? rawTyping(handle)
          : input._tag === 'Gateway'
            ? { encoder: handle.encoder, modes: () => handle.terminal.modes(), ...gateway }
            : undefined
      return (
        <div {...stylex.props(styles.root)}>
          <RenderProfiler id="terminal">
            <TerminalPane screen={rawScreen} state={paneState} controls={controls} onLatest={() => handle.view.scroll('bottom')} {...banner}>
              <TerminalSurface
                screen={rawScreen}
                palette={defaultTerminalPalette}
                typing={sink}
                live={input._tag === 'Attach' ? state._tag === 'Live' : gatewayLive}
                onTypingChange={setTyping}
                resize={input._tag === 'Attach' ? handle.session.resize : undefined}
                scroll={handle.view.scroll}
              />
            </TerminalPane>
          </RenderProfiler>
        </div>
      )
    }
    if (terminal._tag !== 'Observed')
      return (
        <div {...stylex.props(styles.unobserved)}>
          <p role="status" data-terminal-state={terminal._tag === 'Waiting' ? 'loading' : terminal.reason}>
            {terminalUnobservedCopy(terminal)}
          </p>
          {terminal._tag === 'Unavailable' && terminal.retryable === true && source.retryTerminal !== undefined ? (
            <button type="button" onClick={() => source.retryTerminal?.(address.ref)}>Retry</button>
          ) : null}
        </div>
      )
    const screen = terminal.value
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
            state={connected === false ? 'ended' : terminal.freshness !== 'live' ? 'stale' : visible ? 'live' : 'idle'}
            controls={controls}
            {...banner}
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
            {input._tag === 'Gateway' && encoder._tag === 'Ready' ? (
              <TerminalSurface
                screen={screen}
                palette={defaultTerminalPalette}
                typing={{ encoder: encoder.encoder, modes: () => screen.modes, ...gateway }}
                live={gatewayLive}
                onTypingChange={setTyping}
              />
            ) : (
              <DomTerminal screen={screen} palette={defaultTerminalPalette} />
            )}
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
