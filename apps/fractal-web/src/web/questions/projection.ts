import { Schema } from 'effect'
import type { QuestionRequestView } from '@smalltalk/fractal-ui/assistant-ui'
import type { ConversationItem } from '../../conversation/model.ts'
import type { Agent } from '../../data/source.ts'

const AskInput = Schema.Struct({ questions: Schema.Array(Schema.Struct({
  id: Schema.optional(Schema.String), question: Schema.String,
  multi: Schema.optional(Schema.Boolean), recommended: Schema.optional(Schema.Number),
  options: Schema.optional(Schema.Array(Schema.Struct({ label: Schema.String, description: Schema.optional(Schema.String) }))),
})) })

/** Exact native ask axis, not the generic waiting activity or diagnostic reason. */
export const hasPendingQuestion = (agent: Agent): boolean =>
  agent.blockedOn._tag === 'Known' && agent.blockedOn.value === 'human' &&
  agent.ask._tag === 'Known' && agent.ask.value === 'question'

/** The readable timeline supplies ask text. It cannot authorize web settlement (#2315). */
export const pendingQuestionRequests = (items: readonly ConversationItem[], agentLabel: string): readonly QuestionRequestView[] => items.flatMap(item => {
  if (item._tag !== 'ToolCall' || !['ask', 'question/ask'].includes(item.name) || item.status !== 'running' || item.result !== undefined) return []
  let input: typeof AskInput.Type
  try { input = Schema.decodeUnknownSync(AskInput)(item.input) } catch { return [] }
  if (input.questions.length === 0) return []
  return [{
    id: item.callId, source: item.name, agentLabel, askedAt: item.at,
    state: { _tag: 'pending' }, answerable: false,
    readOnlyReason: 'Answer in the agent terminal. This connection does not support answering questions on the web.',
    allowChatRedirect: false,
    questions: input.questions.map((question, index) => ({
      id: question.id ?? `q${index}`, prompt: question.question,
      kind: (question.options?.length ?? 0) === 0 ? 'text' : question.multi === true ? 'multi' : 'single',
      allowOther: true, allowNote: false,
      options: (question.options ?? []).map((option, optionIndex) => ({
        id: `o${optionIndex}`, label: option.label,
        ...(option.description === undefined ? {} : { description: option.description }),
        ...(question.recommended === optionIndex ? { recommended: true } : {}),
      })),
    })),
  }]
})
