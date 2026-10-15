import { Attention, decodeUnknownSync, type AttentionEncoded } from '@smalltalk/st3-client/schema'

// Synthetic wire-contract table: keep all cases here so misc's planned shared request
// fixtures can replace this module without changing predicate, filter or badge tests.
const base = {
  kind: 'attention', id: 'attention/synthetic-ask', source_id: 'agent/synthetic-ask',
  person_id: 'person/operator', attention_kind: 'person-step', title: 'Synthetic request',
  detail: 'Synthetic contract example', priority: 'normal', state: 'open', actions: ['work.done'],
  revision: '1', requested_at: '2026-10-15T12:00:00Z', updated_at: '2026-10-15T12:00:00Z',
} satisfies AttentionEncoded
const cases: readonly { readonly name: string; readonly waiting: boolean; readonly patch: Partial<AttentionEncoded> }[] = [
  { name: 'native person request', waiting: true, patch: {} },
  { name: 'native agent request', waiting: true, patch: { attention_kind: 'agent-request' } },
  { name: 'prompt with advertised answers', waiting: true, patch: { attention_kind: 'harness-prompt', actions: ['prompt.respond'], action_parameters: { 'prompt.respond': { answers: ['allow', 'deny'] } } } },
  { name: 'custom ask', waiting: true, patch: { attention_kind: 'custom.synthetic', actions: ['custom.reply'], custom_form: { fields: [] } } },
  { name: 'update acknowledgement', waiting: false, patch: { update: { type: 'update', version: 1, about: 'mission/synthetic', summary: 'Synthetic update' } } },
  { name: 'unread message', waiting: false, patch: { attention_kind: 'unread-message', actions: ['message.read'] } },
  { name: 'fault', waiting: false, patch: { attention_kind: 'fault' } },
  { name: 'login', waiting: false, patch: { attention_kind: 'harness-login' } },
  { name: 'terminal-only prompt', waiting: false, patch: { attention_kind: 'harness-prompt', actions: [] } },
  { name: 'prompt without answers', waiting: false, patch: { attention_kind: 'harness-prompt', actions: ['prompt.respond'], action_parameters: { 'prompt.respond': { answers: [] } } } },
  { name: 'prompt with unsupported answer schema', waiting: false, patch: { attention_kind: 'harness-prompt', actions: ['prompt.respond'], action_parameters: { 'prompt.respond': { answers: [42] } } } },
  { name: 'answered request', waiting: false, patch: { state: 'resolved' } },
  { name: 'withdrawn action', waiting: false, patch: { actions: [] } },
  { name: 'unsupported request action', waiting: false, patch: { actions: ['review.approve'] } },
  { name: 'unsupported custom action', waiting: false, patch: { attention_kind: 'custom.synthetic', actions: ['work.done'] } },
  { name: 'human gate is not a request', waiting: false, patch: { attention_kind: 'human-gate', actions: ['review.approve'] } },
  { name: 'closed prompt', waiting: false, patch: { state: 'resolved', attention_kind: 'harness-prompt', actions: ['prompt.respond'], action_parameters: { 'prompt.respond': { answers: ['allow'] } } } },
]
export const attentionAnswerFixtures = cases.map(({ name, waiting, patch }) => ({
  name, waiting, item: decodeUnknownSync(Attention, 'strict')({ ...base, ...patch }),
}))
