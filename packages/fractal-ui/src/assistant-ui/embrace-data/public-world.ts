/**
 * Conversation-only TypeScript port of the synthetic fixture world generator.
 * Seed 138 reproduces example/gateway.conversations.json, including the missing Worker 2 result.
 * Unrelated roster/runtime/terminal projections are deliberately not pulled into this workshop.
 * Independent invented data: no captures, private identities, endpoints or observed input.
 */
type Entry<TType extends string, TBody> = {
  readonly id: string
  readonly sequence: number
  readonly revision: number
  readonly final: boolean
  readonly timestamp: string
  readonly role: 'user' | 'assistant' | 'tool'
  readonly type: TType
  readonly body: TBody
}
export type PublicTimelineEntry =
  | Entry<'message', {
      readonly message_id: string
      readonly from: string
      readonly to: string
      readonly title: string
      readonly reply_to: string | null
    }>
  | Entry<'content', { readonly media_type: string; readonly text: string }>
  | Entry<'tool_call', {
      readonly call_id: string
      readonly name: string
      readonly arguments: { readonly path: string }
    }>
  | Entry<'tool_result', {
      readonly call_id: string
      readonly status: 'success' | 'error'
      readonly media_type: string
      readonly content: { readonly rows: number } | { readonly error: string }
    }>

export type PublicConversation = {
  readonly agentId: string
  readonly agentLabel: string
  readonly sessionId: string
  readonly items: readonly PublicTimelineEntry[]
}

export const generatePublicConversations = (seed = 138): readonly PublicConversation[] => {
  if (!Number.isInteger(seed) || seed < 0 || seed > 0xffffffff) {
    throw new Error('Seed must be an unsigned 32-bit integer')
  }
  // The public generator's first LCG value determines its synthetic clock.
  const clock = Date.UTC(2032, 0, 1) + (((Math.imul(seed, 1664525) + 1013904223) >>> 0) % 365) * 86400000
  const at = (seconds = 0) => new Date(clock - seconds * 1000).toISOString()
  const ref = (kind: string, n: number | string) => `${kind}/synthetic-${seed}/${n}`
  return Array.from({ length: 3 }, (_, i): PublicConversation => {
    const agentId = ref('agent', i + 1)
    const call = `sample-call-${seed}-${i + 1}`
    const entry = <TType extends PublicTimelineEntry['type'], const TBody>(
      sequence: number,
      type: TType,
      role: PublicTimelineEntry['role'],
      body: TBody,
      final = true,
    ): Entry<TType, TBody> => ({
      id: ref('timeline-entry', `${i + 1}-${sequence}`), sequence, revision: 1, final,
      timestamp: at(60 - sequence), role, type, body,
    })
    const items: PublicTimelineEntry[] = [
      entry(1, 'message', 'user', {
        message_id: ref('message', i + 1), from: 'person/operator', to: agentId,
        title: 'Build a sample', reply_to: null,
      }),
      entry(2, 'content', 'user', {
        media_type: 'text/plain', text: 'Prepare a synthetic sample with four rows.',
      }),
      entry(3, 'tool_call', 'assistant', {
        call_id: call, name: 'read', arguments: { path: '/srv/work/sample/input.json' },
      }),
    ]
    if (i !== 1) items.push(entry(4, 'tool_result', 'tool', {
      call_id: call, status: i === 2 ? 'error' : 'success', media_type: 'application/json',
      content: i === 2 ? { error: 'Synthetic input unavailable' } : { rows: 4 },
    }))
    items.push(entry(5, 'content', 'assistant', {
      media_type: 'text/plain',
      text: i === 1 ? 'Waiting for the synthetic read.' : 'The sample inspection is complete.',
    }, i !== 1))
    return { agentId, agentLabel: `Worker ${i + 1}`, sessionId: ref('session', i + 1), items }
  })
}
