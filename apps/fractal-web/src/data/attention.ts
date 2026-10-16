import type { Attention } from '@smalltalk/st3-client/schema'

/**
 * Mirrors the planned st3_ui_model::requests::RequestView::is_waiting_for_answer:
 * an open native request, prompt or custom ask with an advertised supported answer.
 * Attention feeds are already scoped to the current person. Updates/read acknowledgements,
 * approvals, faults, login cards and terminal-only prompts are not answerable requests.
 * Classify wire discriminants/actions, never presentation text or harness blocked_on.
 */
export const isWaitingForAnswer = (item: Attention): boolean => {
  if (item.state !== 'open' || item.update !== undefined) return false
  if (item.attention_kind === 'person-step' || item.attention_kind === 'agent-request')
    return item.actions.includes('work.done')
  if (item.attention_kind.startsWith('custom.')) return item.actions.includes('custom.reply')
  if (item.attention_kind !== 'harness-prompt' || !item.actions.includes('prompt.respond')) return false
  const parameters = item.action_parameters?.['prompt.respond']
  if (typeof parameters !== 'object' || parameters === null || !('answers' in parameters)) return false
  return Array.isArray(parameters.answers) && parameters.answers.some((answer: unknown) => typeof answer === 'string' && answer.length > 0)
}
