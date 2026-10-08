import type { CollectionFrame, TimelineEntry } from '@smalltalk/st3-client'
import { decodeConversationChunk } from '@st3/sdk/effect'
import { describe, expect, it } from 'vitest'
import { LiveTimeline } from '../conversation/fromTimeline.ts'
import { mapConversationFeed, prepareTranscriptTurns } from './conversationTranscript.ts'

// Invented payloads reproduce the captured page's shape without committing private transcripts:
// 92 projected items, 88 before the sole user Text, 12 reasoning labels, 36 tool starts.
const richProfile = (): Extract<CollectionFrame, { kind: 'conversation' }> => {
  const items: TimelineEntry[] = []
  const text = (value: string, role: TimelineEntry['role'] = 'assistant') => {
    const sequence = items.length
    items.push({ id: `timeline-entry/profile-${sequence}`, sequence, revision: 1, final: true,
      timestamp: '2026-10-03T00:00:00Z', type: 'content', role,
      body: { media_type: 'text/plain', text: value } })
  }
  for (let index = 0; index < 12; index++) text(`[reasoning]\nThought ${index}`)
  for (let index = 0; index < 36; index++) {
    const sequence = items.length
    items.push({ id: `timeline-entry/profile-${sequence}`, sequence, revision: 1, final: true,
      timestamp: '2026-10-03T00:00:00Z', type: 'tool_call', role: 'assistant',
      body: { call_id: `call-${index}`, name: 'read', arguments: { path: 'example.ts' } } })
    text(`[unrecognized omp entry \`custom\`]\n${JSON.stringify({ raw: { type: 'custom', customType: 'tool_execution_start', data: { toolCallId: `call-${index}` } } })}`, 'system')
  }
  for (let index = 0; index < 4; index++) text(`Earlier assistant answer ${index}`)
  text('A real human prompt.', 'user')
  text('[unrecognized omp entry `custom`]\n{"raw":{"type":"custom","customType":"session_exit"}}', 'system')
  text('Later answer 1')
  text('Later answer 2')
  return { kind: 'conversation', collection: 'conversation', id: 'follow/profile',
    session_id: 'session/profile', replace: true, has_more: true, items }
}

describe('rich page decode, fold and transcript mapping', () => {
  it.each([
    ['developer', '`', true],
    ['developer', "'", true],
    ['system-reminder', '`', true],
    ['system-reminder', "'", true],
    ['future-role', '`', false],
  ] as const)('handles native role %s with %s markers without exposing protocol JSON', (role, quote, omitted) => {
    const timeline = new LiveTimeline()
    const page: Extract<CollectionFrame, { kind: 'conversation' }> = {
      kind: 'conversation', collection: 'conversation', id: 'follow/roles', session_id: 'session/roles',
      replace: true, has_more: false, items: [{
        id: 'timeline-entry/roles', sequence: 0, revision: 1, final: true, role: 'system',
        timestamp: '2026-10-03T00:00:00Z', type: 'content', body: {
          media_type: 'text/plain',
          text: `[unrecognized omp message role ${quote}${role}${quote}]\n{"raw":{"message":{"role":"${role}","content":"synthetic-role-payload"}}}`,
        },
      }],
    }
    timeline.apply(decodeConversationChunk(page))
    const state = mapConversationFeed({
      _tag: 'Observed', freshness: 'live',
      value: { items: timeline.project().items, hasOlder: false, observation: { empty: false } },
    }, {})
    expect(state._tag).toBe('Observed')
    if (state._tag !== 'Observed') return
    expect(state.items).toHaveLength(omitted ? 0 : 1)
    if (!omitted) expect(state.items[0]).toMatchObject({ _tag: 'Notice', text: 'An event this view cannot show yet.' })
    expect(JSON.stringify(state)).not.toContain('synthetic-role-payload')
    expect(JSON.stringify(state)).not.toContain('[unrecognized omp')
    expect(state.filteredEmpty).toBe(omitted)
  })

  it('recovers reasoning, omits only known internals, retains a neutral unknown row and native history', () => {
    const timeline = new LiveTimeline()
    timeline.apply(decodeConversationChunk(richProfile()))
    const items = timeline.project().items
    expect(items).toHaveLength(92)
    expect(items.findIndex(item => item._tag === 'Text' && item.role === 'user')).toBe(88)
    const turns = prepareTranscriptTurns(items, { firstTurnComplete: !timeline.hasOlder })
    const kept = turns.flatMap(turn => turn.prompt === undefined ? turn.items : [turn.prompt, ...turn.items])
    expect(turns).toHaveLength(2)
    expect(turns[0]!.prompt).toBeUndefined()
    expect(kept.filter(item => item._tag === 'Reasoning')).toHaveLength(12)
    expect(kept.filter(item => item._tag === 'Text').some(item => item.text.includes('[reasoning]'))).toBe(false)
    expect(kept.filter(item => item._tag === 'UnknownEvent')).toHaveLength(0)
    expect(kept.filter(item => item._tag === 'Notice')).toEqual([
      { _tag: 'Notice', id: 'timeline-entry/profile-89', kind: 'event', text: 'An event this view cannot show yet.', at: '2026-10-03T00:00:00.000Z' },
    ])
    expect(JSON.stringify(kept)).not.toContain('session_exit')
    expect(JSON.stringify(kept)).not.toContain('tool_execution_start')
    expect(kept.some(item => item._tag === 'Notice' && item.kind === 'truncation')).toBe(false)
    const active = mapConversationFeed({ _tag: 'Observed', freshness: 'live', value: {
      items, hasOlder: timeline.hasOlder, observation: timeline.observation,
    } }, {})
    expect(active).toMatchObject({ _tag: 'Observed', history: { _tag: 'HasOlder' } })
    expect(active._tag === 'Observed' && active.turns).toHaveLength(2)
  })
})
