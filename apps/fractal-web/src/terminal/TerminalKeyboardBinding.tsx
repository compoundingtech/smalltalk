import { RegistryContext, useAtomRefresh, useAtomValue } from '@effect/atom-react'
import type { TerminalModes } from '@smalltalk/st3-client/schema'
import * as stylex from '@stylexjs/stylex'
import { Effect, Schema } from 'effect'
import * as Atom from 'effect/reactivity/Atom'
import React from 'react'
import { Button } from 'react-aria-components'

import { scale, tokens } from '../ui-compat/tokens.stylex.ts'

import { loadGhosttyKeyboard, type TerminalKeyEncoder } from './ghosttyKeyboard.ts'
import type { TerminalInputPort, TerminalInputPortFactory } from './orderedTerminalInput.ts'
import { TerminalKeyboard } from './TerminalKeyboard.tsx'

/** Props for binding ordered input to one observed terminal incarnation. */
export interface TerminalKeyboardBindingProps {
  readonly terminalRef: string
  readonly incarnation: string
  readonly modes: TerminalModes
  readonly factory?: TerminalInputPortFactory | undefined
  readonly unavailableReason?: string | undefined
}

/** Parent supplies the optional admitted source factory. Missing production support never opens a socket. */
export const TerminalKeyboardBinding = (props: TerminalKeyboardBindingProps) =>
  props.factory === undefined || props.unavailableReason !== undefined ? (
    <TerminalKeyboard
      modes={props.modes}
      unavailableReason={
        props.unavailableReason ?? 'Ordered terminal input is not available from this producer'
      }
    />
  ) : (
    <SupportedKeyboard {...props} factory={props.factory} />
  )

/** Preparation stopped because the component released its request before any socket opened. */
class TerminalInputPreparationCancelled extends Schema.TaggedError<TerminalInputPreparationCancelled>()(
  'TerminalInputPreparationCancelled',
  {},
) {}

/** The admitted source port or the native encoder failed to prepare. */
class TerminalInputPreparationFailed extends Schema.TaggedError<TerminalInputPreparationFailed>()(
  'TerminalInputPreparationFailed',
  { cause: Schema.Unknown },
) {}

interface PreparedKeyboard {
  readonly port: TerminalInputPort
  readonly encoder: TerminalKeyEncoder
}

const SupportedKeyboard = ({
  terminalRef,
  incarnation,
  modes,
  factory,
}: TerminalKeyboardBindingProps & { readonly factory: TerminalInputPortFactory }) => {
  const registry = React.useContext(RegistryContext)
  const resource = React.useMemo(
    () =>
      Atom.make(
        Effect.acquireRelease(
          Effect.tryPromise({
            try: async (signal): Promise<PreparedKeyboard> => {
              const port = await factory({ terminalRef, incarnation, registry })
              if (signal.aborted) {
                port.close()
                throw new TerminalInputPreparationCancelled()
              }
              let encoder: TerminalKeyEncoder
              try {
                encoder = await loadGhosttyKeyboard()
              } catch (cause) {
                port.close()
                throw new TerminalInputPreparationFailed({
                  _tag: 'TerminalInputPreparationFailed',
                  cause,
                })
              }
              if (signal.aborted) {
                port.close()
                encoder.dispose()
                throw new TerminalInputPreparationCancelled()
              }
              return { port, encoder }
            },
            catch: (cause) =>
              cause instanceof TerminalInputPreparationCancelled ||
              cause instanceof TerminalInputPreparationFailed
                ? cause
                : new TerminalInputPreparationFailed({
                    _tag: 'TerminalInputPreparationFailed',
                    cause,
                  }),
          }),
          (prepared) =>
            Effect.sync(() => {
              prepared.port.close()
              prepared.encoder.dispose()
            }),
        ),
      ).pipe(Atom.setIdleTTL(0)),
    [factory, registry, terminalRef, incarnation],
  )
  const result = useAtomValue(resource)
  const refresh = useAtomRefresh(resource)
  if (result._tag === 'Success' && !result.waiting)
    return (
      <TerminalKeyboard
        modes={modes}
        port={result.value.port}
        encoder={result.value.encoder}
        onNewSession={refresh}
      />
    )
  return (
    <div>
      <TerminalKeyboard
        modes={modes}
        unavailableReason={
          result._tag === 'Initial' || result.waiting
            ? 'Preparing native terminal input…'
            : 'Terminal input could not be prepared. No keystrokes were sent.'
        }
      />
      {result._tag === 'Failure' && (
        <Button onPress={refresh} {...stylex.props(styles.retry)}>
          New input session
        </Button>
      )}
    </div>
  )
}

const styles = stylex.create({
  retry: {
    margin: scale.space2,
    padding: scale.space1,
    fontSize: '0.75rem',
    color: tokens['--ds-gray-1000'],
    backgroundColor: 'transparent',
    borderWidth: '1px',
    borderStyle: 'solid',
    borderColor: tokens['--ds-gray-alpha-400'],
    outlineColor: tokens['--ds-focus-color'],
  },
})
