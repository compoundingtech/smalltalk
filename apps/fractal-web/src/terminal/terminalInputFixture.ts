import type { TerminalModes } from '@smalltalk/st3-client/schema'
import * as Atom from 'effect/reactivity/Atom'
import type * as AtomRegistry from 'effect/reactivity/AtomRegistry'

import { makeOrderedTerminalInput, type OrderedTerminalInput } from './orderedTerminalInput.ts'

/** Plain legacy-mode terminal with bracketed paste, shared by encoder and keyboard proofs. */
export const keyboardTestModes: TerminalModes = {
  alternate_screen: false,
  application_cursor: false,
  application_keypad: false,
  bracketed_paste: true,
  kitty_keyboard: 0,
  focus_events: false,
  mouse_encoding: 'default',
  mouse_tracking: 'none',
}

/** Local deterministic peer over the production lifecycle core. Never installed as a live source. */
export const makeTerminalInputFixture = ({
  registry,
  localOwner = true,
  controlGranted = true,
  acknowledge = true,
}: {
  readonly registry: AtomRegistry.AtomRegistry
  readonly localOwner?: boolean
  readonly controlGranted?: boolean
  readonly acknowledge?: boolean
}) => {
  const writes = Atom.make<
    ReadonlyArray<{ readonly seq: number; readonly bytes: ReadonlyArray<number> }>
  >([]).pipe(Atom.keepAlive)
  let input: OrderedTerminalInput | undefined
  let held = false
  let opened = false
  let expected = 7
  const port = makeOrderedTerminalInput({
    registry,
    localOwner,
    controlGranted,
    transport: {
      holdFollow: () => {
        held = true
      },
      open: () => {
        if (!held) throw new Error('Fixture peer refuses input without a held terminal follow')
        opened = true
        input?.opened(expected)
      },
      send: ({ seq, bytes }) => {
        if (!opened || seq !== expected) throw new Error('Fixture peer refuses unordered input')
        expected += 1
        registry.set(writes, [...registry.get(writes), { seq, bytes: Array.from(bytes) }])
        if (acknowledge) input?.acknowledged(seq)
      },
      close: () => {
        opened = false
      },
      releaseFollow: () => {
        held = false
      },
    },
  })
  input = port
  return {
    port,
    writes,
    disconnect: () => {
      opened = false
      held = false
      port.invalidate('The collections socket disconnected')
    },
    dispose: () => port.close(),
  }
}
