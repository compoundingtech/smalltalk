import { RegistryContext, useAtomValue } from '@effect/atom-react'
import type { TerminalModes } from '@smalltalk/st3-client/schema'
import * as stylex from '@stylexjs/stylex'
import * as Atom from 'effect/reactivity/Atom'
import React from 'react'
import { FocusScope, useKeyboard } from 'react-aria'
import { Button } from 'react-aria-components'

import { scale, tokens } from '../ui-compat/tokens.stylex.ts'

import type { BrowserTerminalKey, TerminalKeyEncoder } from './ghosttyKeyboard.ts'
import type { TerminalInputPort, TerminalInputState } from './orderedTerminalInput.ts'

const absent = Atom.make<TerminalInputState>({
  _tag: 'Closed',
  reason: 'Ordered terminal input is not available from this producer',
  uncertain: false,
}).pipe(Atom.keepAlive)

/** Configuration for the native terminal input surface. */
export interface TerminalKeyboardProps {
  readonly modes: TerminalModes
  readonly port?: TerminalInputPort
  readonly encoder?: TerminalKeyEncoder
  readonly unavailableReason?: string
  readonly layout?: ReadonlyMap<string, string>
  /** Explicit recovery creates a NEW lifetime. It must not replay bytes or automatically arm it. */
  readonly onNewSession?: () => void
}

/** Native IME/clipboard target, React Aria keyboard ownership, scoped focus; no emulator renderer. */
export const TerminalKeyboard = ({
  modes,
  port,
  encoder,
  unavailableReason,
  layout,
  onNewSession,
}: TerminalKeyboardProps) => {
  const registry = React.useContext(RegistryContext)
  const state = useAtomValue(port?.state ?? absent)
  const composing = React.useRef(false)
  const committed = React.useRef<string | undefined>(undefined)
  const [notice, setNotice] = React.useState<string | undefined>(undefined)
  const id = React.useId()
  const disabledReason =
    unavailableReason ??
    (port === undefined
      ? 'Ordered terminal input is not available from this producer'
      : encoder === undefined
        ? 'Native terminal key encoder is unavailable'
        : undefined)
  const ready = disabledReason === undefined && state._tag === 'Ready'
  const lifecycleRef = React.useCallback(
    (node: HTMLDivElement | null) => {
      if (node === null || port === undefined) return
      const hide = () => {
        if (document.visibilityState === 'hidden') port.close()
      }
      const pageHide = () => port.close()
      document.addEventListener('visibilitychange', hide)
      window.addEventListener('pagehide', pageHide)
      return () => {
        document.removeEventListener('visibilitychange', hide)
        window.removeEventListener('pagehide', pageHide)
        // StrictMode's pre-arm ref probe must not turn an idle session into a closed one.
        if (registry.get(port.state)._tag !== 'Idle') port.close()
      }
    },
    [port, registry],
  )
  const send = (bytes: Uint8Array) => {
    if (!ready || port === undefined || bytes.length === 0) return
    setNotice(undefined)
    // Port rejections may carry transport text; the lifecycle state already states the fixed reason.
    void port.send(bytes).catch(() => setNotice('Terminal input could not be sent. No keystrokes were replayed.'))
  }
  const { keyboardProps } = useKeyboard({
    isDisabled: !ready,
    onKeyDown: (event) => {
      if (event.ctrlKey && (event.key === '\\' || event.code === 'Backslash')) {
        event.preventDefault()
        port?.close()
        return
      }
      if (composing.current || event.nativeEvent.isComposing || event.keyCode === 229) {
        event.continuePropagation()
        return
      }
      committed.current = undefined
      // Clipboard and terminal-local Find remain platform/parent-owned. Ctrl-C is terminal input.
      if (
        (event.metaKey || event.ctrlKey) &&
        ['c', 'v', 'f'].includes(event.key.toLowerCase()) &&
        (event.metaKey || event.shiftKey || event.key.toLowerCase() !== 'c')
      ) {
        event.continuePropagation()
        return
      }
      if (encoder === undefined) return
      const key: BrowserTerminalKey = {
        code: event.code,
        key: event.key,
        action: event.repeat ? 'repeat' : 'press',
        shiftKey: event.shiftKey,
        ctrlKey: event.ctrlKey,
        altKey: event.altKey,
        metaKey: event.metaKey,
        capsLock: event.getModifierState('CapsLock'),
        numLock: event.getModifierState('NumLock'),
        altGraph: event.getModifierState('AltGraph'),
        unshifted: layout?.get(event.code),
      }
      event.preventDefault()
      send(encoder.key({ event: key, modes }))
    },
    onKeyUp: (event) => {
      if (composing.current || event.nativeEvent.isComposing || encoder === undefined) return
      if (
        (event.metaKey || event.ctrlKey) &&
        ['c', 'v', 'f'].includes(event.key.toLowerCase()) &&
        (event.metaKey || event.shiftKey || event.key.toLowerCase() !== 'c')
      )
        return
      send(
        encoder.key({
          event: {
            code: event.code,
            key: event.key,
            action: 'release',
            shiftKey: event.shiftKey,
            ctrlKey: event.ctrlKey,
            altKey: event.altKey,
            metaKey: event.metaKey,
            capsLock: event.getModifierState('CapsLock'),
            numLock: event.getModifierState('NumLock'),
            altGraph: event.getModifierState('AltGraph'),
            unshifted: layout?.get(event.code),
          },
          modes,
        }),
      )
    },
  })
  return (
    <div ref={lifecycleRef} {...stylex.props(styles.root)} data-terminal-input-state={state._tag}>
      <div {...stylex.props(styles.header)}>
        <label htmlFor={id} {...stylex.props(styles.label)}>
          Terminal keyboard
        </label>
        {disabledReason === undefined && state._tag === 'Idle' && (
          <Button onPress={() => port?.open()} {...stylex.props(styles.button)}>
            Enable input
          </Button>
        )}
        {ready && (
          <Button onPress={() => port?.close()} {...stylex.props(styles.button)}>
            Disable input
          </Button>
        )}
        {disabledReason === undefined && state._tag === 'Closed' && onNewSession !== undefined && (
          <Button onPress={onNewSession} {...stylex.props(styles.button)}>
            New input session
          </Button>
        )}
      </div>
      <FocusScope key={ready ? 'entered' : 'idle'} contain={ready} restoreFocus autoFocus={ready}>
        <textarea
          {...keyboardProps}
          id={id}
          aria-describedby={`${id}-status`}
          disabled={!ready}
          rows={1}
          autoCapitalize="off"
          autoCorrect="off"
          spellCheck={false}
          {...stylex.props(styles.input)}
          placeholder={
            ready ? 'Type into the terminal; Ctrl-\\ leaves input' : 'Terminal input unavailable'
          }
          onBlur={() => {
            if (ready) port?.close()
          }}
          onCompositionStart={() => {
            composing.current = true
            committed.current = undefined
          }}
          onCompositionEnd={(event) => {
            composing.current = false
            committed.current = event.data
            event.currentTarget.value = ''
            if (encoder !== undefined) send(encoder.text(event.data))
          }}
          onBeforeInput={(event) => {
            const native = event.nativeEvent
            if (!(native instanceof InputEvent) || native.isComposing || composing.current) return
            if (native.inputType !== 'insertText' && native.inputType !== 'insertFromComposition')
              return
            event.preventDefault()
            if (native.data === committed.current) {
              committed.current = undefined
              return
            }
            committed.current = undefined
            if (native.data !== null && encoder !== undefined) send(encoder.text(native.data))
          }}
          onInput={(event) => {
            if (!composing.current) event.currentTarget.value = ''
          }}
          onPaste={(event) => {
            if (!ready || encoder === undefined || composing.current) return
            event.preventDefault()
            send(encoder.paste({ text: event.clipboardData.getData('text/plain'), modes }))
          }}
        />
      </FocusScope>
      <p id={`${id}-status`} role="status" {...stylex.props(styles.status)}>
        {disabledReason ??
          (state._tag === 'Opening'
            ? 'Opening ordered input…'
            : state._tag === 'Closed'
              ? `${state.reason}${state.uncertain ? '. Delivery is uncertain; no bytes were replayed.' : ''}${onNewSession === undefined ? ' Reopen this terminal to create a new input session.' : ''}`
              : state._tag === 'Ready'
                ? `${state.pending > 0 ? `${state.pending} batch awaiting transport acknowledgement. ` : ''}Native Ghostty keys; mode-aware paste. Ctrl-\\ leaves input.`
                : 'Enable input explicitly. Reconnecting never replays prior keystrokes.')}
      </p>
      {notice !== undefined && (
        <p role="alert" {...stylex.props(styles.error)}>
          {notice}
        </p>
      )}
    </div>
  )
}

const styles = stylex.create({
  root: {
    borderTopWidth: '1px',
    borderTopStyle: 'solid',
    borderTopColor: tokens['--ds-gray-alpha-400'],
    padding: scale.space2,
    fontSize: '0.75rem',
    color: tokens['--ds-gray-1000'],
  },
  header: { display: 'flex', alignItems: 'center', gap: scale.space2, marginBottom: scale.space1 },
  label: { fontWeight: 500 },
  button: {
    height: '1.5rem',
    paddingInline: scale.space2,
    fontSize: '0.75rem',
    color: tokens['--ds-gray-1000'],
    backgroundColor: {
      default: 'transparent',
      ':is([data-hovered])': tokens['--ds-gray-alpha-100'],
    },
    borderWidth: '1px',
    borderStyle: 'solid',
    borderColor: tokens['--ds-gray-alpha-400'],
    cursor: 'pointer',
    outlineStyle: { default: 'none', ':is([data-focus-visible])': 'solid' },
    outlineColor: tokens['--ds-focus-color'],
    outlineWidth: '2px',
  },
  input: {
    display: 'block',
    width: '100%',
    boxSizing: 'border-box',
    resize: 'none',
    padding: scale.space2,
    color: tokens['--ds-gray-1000'],
    backgroundColor: tokens['--ds-background-100'],
    borderWidth: '1px',
    borderStyle: 'solid',
    borderColor: tokens['--ds-gray-alpha-400'],
    fontFamily: 'monospace',
    outlineColor: tokens['--ds-focus-color'],
    opacity: { default: 1, ':disabled': 0.5 },
  },
  status: { margin: 0, marginTop: scale.space1, color: tokens['--ds-gray-900'], lineHeight: 1.5 },
  error: { margin: 0, marginTop: scale.space1, color: tokens['--ds-red-900'] },
})
