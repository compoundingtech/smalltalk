import type { TimelineEntry } from '@smalltalk/st3-client'

import type { CastAgent } from './cast.ts'
import type { FactoryContext } from './context.ts'
import { thread, turn } from './factories/turn.ts'
import { fork, pick, rngFromSeed } from './rng.ts'
import type { ConversationHistory } from './slice.ts'
import { files, taskTitles } from './vocabulary/index.ts'

export const HUGE_TURNS = 10_000
export const HUGE_COMMITTED_TURNS = 400
export const ENTRIES_PER_HISTORY_TURN = 4

/** Random access: seed each exchange separately, never generate or retain the entire history. */
export const generateConversationRange = (
  ctx: FactoryContext,
  member: CastAgent,
  history: ConversationHistory,
  firstSequence: number,
  lastSequence: number,
): TimelineEntry[] => {
  const first = Math.max(1, firstSequence)
  const last = Math.min(history.total_entries, lastSequence)
  const person = ctx.cast.people[0]
  if (person === undefined) throw new Error('seeded conversation history requires a cast person')
  const root = rngFromSeed(history.seed)
  const items: TimelineEntry[] = []
  for (let exchange = Math.floor((first - 1) / ENTRIES_PER_HISTORY_TURN); exchange <= Math.floor((last - 1) / ENTRIES_PER_HISTORY_TURN); exchange += 1) {
    const c: FactoryContext = { ...ctx, world: history.world, rng: fork(root, `conversation/huge/${member.key}/${exchange}`) }
    const file = pick(c.rng, files)
    const task = pick(c.rng, taskTitles)
    const entries = turn(c, thread(member, exchange * ENTRIES_PER_HISTORY_TURN), {
      atMs: -(history.total_turns - exchange) * 30_000,
      from: { _tag: 'person', person },
      text: `Migration batch ${exchange + 1}: ${task}. Check ${file} before moving to the next caller.`,
      steps: [{ _tag: 'say', text: `Batch ${exchange + 1}: inspected ${file}, preserved the session argument and checked the caller tests.` }],
      status: 'completed',
      stepMs: 1_000,
    })
    items.push(...entries.filter(({ sequence }) => sequence >= first && sequence <= last))
  }
  return items
}
