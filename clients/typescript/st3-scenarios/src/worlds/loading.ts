import type { Selector } from '../kit/slice.ts'
import type { WorldDefinition } from '../kit/world.ts'
import { fleetMidRefactor } from './fleetMidRefactor.ts'

/** Discovery succeeds while all initial resource replies remain outstanding. */
export const loading: WorldDefinition = {
  ...fleetMidRefactor,
  id: 'loading',
  title: 'Waiting for first replies',
  narrative: 'Discovery succeeded. Collection, conversation and terminal requests are still waiting for their first replies.',
  slices: (ctx) => {
    const base = fleetMidRefactor.slices(ctx)
    const selectors: Selector[] = [
      ...(['agents', 'missions', 'work', 'attention'] as const).map((collection) => ({ collection })),
      ...base.conversation.state.threads.map((thread) => ({ collection: 'conversation' as const, conversation: thread.agent })),
      ...base.terminal.state.terminals.map((record) => ({ collection: 'terminal' as const, terminal: record.terminal })),
    ]
    return {
      roster: { ...base.roster, loading: true, timeline: [] },
      details: { ...base.details, loading: true, timeline: [] },
      attention: { ...base.attention, loading: true, timeline: [] },
      conversation: { ...base.conversation, loading: true, timeline: [] },
      terminal: { ...base.terminal, loading: true, timeline: [] },
      sync: {
        ...base.sync,
        variant: 'requested',
        state: {
          ...base.sync.state,
          expected: selectors.map((selector) => ({
            surface: selector.collection === 'conversation' ? `conversation:${selector.conversation}` : selector.collection === 'terminal' ? `terminal:${selector.terminal}` : selector.collection,
            at_ms: 0,
            status: { _tag: 'Requested', since: 0 },
            compare: 'shape',
          })),
        },
        timeline: selectors.map((selector) => ({ _tag: 'hold', at_ms: 0, store: 0, selector })),
      },
    }
  },
}
