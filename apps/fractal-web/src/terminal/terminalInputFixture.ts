import * as Atom from 'effect/reactivity/Atom'
import type * as AtomRegistry from 'effect/reactivity/AtomRegistry'

import { makeActionTerminalInput, type TerminalInputOutcome } from './actionTerminalInput.ts'
import type { TerminalInputWire } from './terminalInputWire.ts'

/**
 * Local deterministic gateway over the production input session. Never installed as a live
 * source. Each submitted action waits for `answer` unless `autoDeliver` settles it at once.
 */
export const makeTerminalInputFixture = ({
  registry,
  autoDeliver = true,
}: {
  readonly registry: AtomRegistry.AtomRegistry
  readonly autoDeliver?: boolean
}) => {
  const writes = Atom.make<ReadonlyArray<TerminalInputWire>>([]).pipe(Atom.keepAlive)
  const answers: Array<(outcome: TerminalInputOutcome) => void> = []
  const port = makeActionTerminalInput({
    registry,
    submit: (wire) => {
      registry.set(writes, [...registry.get(writes), wire])
      if (autoDeliver) return Promise.resolve({ _tag: 'Delivered' })
      const answer = Promise.withResolvers<TerminalInputOutcome>()
      answers.push(answer.resolve)
      return answer.promise
    },
  })
  return {
    port,
    writes,
    /** Settle the oldest unanswered action. */
    answer: (outcome: TerminalInputOutcome) => answers.shift()?.(outcome),
    dispose: () => port.close(),
  }
}
