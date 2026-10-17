import type { Agent as AgentRow, Attention } from '@smalltalk/st3-client/schema'
import * as Option from 'effect/Option'

/** Open st attention about an agent: it raised, requested, or owns or shows the card in its conversation, or the card is on one of its current missions. */
export const attentionAboutAgent = ({
  card,
  ref,
  missionIds,
}: {
  readonly card: Attention
  readonly ref: string
  readonly missionIds: ReadonlySet<string>
}): boolean =>
  card.state === 'open' &&
  (card.source_id === ref ||
    card.requester_id === ref ||
    card.conversation_id === ref ||
    card.conversation_ids?.some((id) => id === ref) === true ||
    (card.mission_id !== undefined && missionIds.has(card.mission_id)))

/** Missions of the agent's current work, as st names them on the agent row. */
export const currentMissionIds = (row: AgentRow | undefined): ReadonlySet<string> =>
  new Set(row?.current_work?.map((work) => work.mission_id) ?? [])

/** st reports an ask-tool question: the seat's structured ask kind is `question`. st's harness writer sets
 * `ask` only while `blocked_on` is `human` (st-drivers harness_state), so `ask` alone decides. */
export const asksQuestion = (row: AgentRow): boolean => Option.getOrUndefined(row.ask) === 'question'

/** "Waiting for answer" reads only st: an ask-tool question OR open attention, never Fractal's status heuristics. */
export const askingAgentRefs = ({
  rows,
  attention,
}: {
  readonly rows: readonly AgentRow[]
  readonly attention: readonly Attention[]
}): ReadonlySet<string> => {
  const open = attention.filter((card) => card.state === 'open')
  const asking = new Set<string>()
  for (const row of rows) {
    if (asksQuestion(row)) asking.add(row.id)
    else if (open.length > 0) {
      const missionIds = currentMissionIds(row)
      if (open.some((card) => attentionAboutAgent({ card, ref: row.id, missionIds }))) asking.add(row.id)
    }
  }
  return asking
}
