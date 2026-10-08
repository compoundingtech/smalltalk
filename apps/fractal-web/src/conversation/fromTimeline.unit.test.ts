import { TimelineEntry } from '@smalltalk/st3-client/schema'
import { Schema } from 'effect'
import { describe, expect, it } from 'vitest'
import { LiveTimeline } from './fromTimeline.ts'

describe('native conversation page boundaries', () => {
  it('retains the older edge across newer deltas and resets it on replace', () => {
    const timeline = new LiveTimeline()
    timeline.apply({ entries: [], replace: true, hasMore: false, observation: { empty: true } })
    timeline.apply({ entries: [], replace: false, hasMore: true })
    expect(timeline.hasOlder).toBe(false)
    expect(timeline.observation).toEqual({ empty: true })
    timeline.apply({ entries: [], replace: true, hasMore: true, observation: { empty: false } })
    expect(timeline.hasOlder).toBe(true)
    timeline.apply({ entries: [], replace: false, hasMore: false })
    expect(timeline.hasOlder).toBe(true)
  })

  it('does not infer empty from a filtered projection or missing page evidence', () => {
    const timeline = new LiveTimeline()
    timeline.apply({ entries: [], replace: true, hasMore: false })
    expect(timeline.observation).toBeUndefined()
    timeline.apply({ entries: [], replace: true, hasMore: false, observation: { empty: true } })
    timeline.apply({ entries: [], replace: false, hasMore: false, observation: { empty: false } })
    expect(timeline.observation).toEqual({ empty: false })
  })
})

describe('mailbox deliveries', () => {
  const entry = (id: string, sequence: number, type: string, body: unknown, role = 'user') =>
    Schema.decodeUnknownSync(TimelineEntry)({
      id, sequence, type, body, role, revision: 1, final: true,
      timestamp: '2026-10-03T00:00:00Z',
    })

  it('shows mailbox mail once at its original position, not again as a later harness delivery', () => {
    const timeline = new LiveTimeline()
    timeline.apply({
      replace: true, hasMore: false,
      entries: [
        entry('timeline-entry/mail/message', 1, 'message', {
          message_id: 'message/abc', from: 'person/operator', to: 'agent/example',
        }),
        entry('timeline-entry/mail/content', 2, 'content', { media_type: 'text/plain', text: 'hello' }),
        entry('timeline-entry/reply', 3, 'content', { media_type: 'text/plain', text: 'reply' }, 'assistant'),
        entry('timeline-entry/native/header', 4, 'message', { message_id: 'native/turn' }),
        entry('timeline-entry/native/content', 5, 'content', {
          media_type: 'text/plain',
          text: `<smalltalk-message id="abc" graph="message/abc" from="person/operator" to="agent/example">
hello
</smalltalk-message>
The person reads replies in st, not in the agent's session.`,
        }),
      ],
    })
    expect(timeline.project().items.map((item) => item.id)).toEqual([
      'timeline-entry/mail/message', 'timeline-entry/mail/content', 'timeline-entry/reply',
    ])
  })

  it('keeps mailbox message content that quotes a shown delivery verbatim', () => {
    const timeline = new LiveTimeline()
    timeline.apply({
      replace: true, hasMore: false,
      entries: [
        entry('timeline-entry/first/message', 1, 'message', {
          message_id: 'message/abc', from: 'person/operator', to: 'agent/example',
        }),
        entry('timeline-entry/first/content', 2, 'content', { media_type: 'text/plain', text: 'hello' }),
        entry('timeline-entry/quote/message', 3, 'message', {
          message_id: 'message/quote', from: 'person/operator', to: 'agent/example',
        }),
        entry('timeline-entry/quote/content', 4, 'content', {
          media_type: 'text/plain',
          text: `<smalltalk-message graph="message/abc">I received this envelope</smalltalk-message>
and copied it into my reply`,
        }),
      ],
    })
    expect(timeline.project().items.map((item) => item.id)).toEqual([
      'timeline-entry/first/message', 'timeline-entry/first/content',
      'timeline-entry/quote/message', 'timeline-entry/quote/content',
    ])
    const quoted = timeline.project().items.at(-1)
    expect(quoted?._tag === 'Text' ? quoted.text : '').toBe(
      `<smalltalk-message graph="message/abc">I received this envelope</smalltalk-message>
and copied it into my reply`,
    )
  })

  it('keeps an envelope-only mailbox message instead of dropping its whole content row', () => {
    const timeline = new LiveTimeline()
    timeline.apply({
      replace: true, hasMore: false,
      entries: [
        entry('timeline-entry/first/message', 1, 'message', {
          message_id: 'message/abc', from: 'person/operator', to: 'agent/example',
        }),
        entry('timeline-entry/first/content', 2, 'content', { media_type: 'text/plain', text: 'hello' }),
        entry('timeline-entry/only/message', 3, 'message', {
          message_id: 'message/only', from: 'person/operator', to: 'agent/example',
        }),
        entry('timeline-entry/only/content', 4, 'content', {
          media_type: 'text/plain',
          text: '<smalltalk-message graph="message/abc">the quoted mail</smalltalk-message>',
        }),
      ],
    })
    expect(timeline.project().items.map((item) => item.id)).toEqual([
      'timeline-entry/first/message', 'timeline-entry/first/content',
      'timeline-entry/only/message', 'timeline-entry/only/content',
    ])
  })

  it('keeps directly typed harness turns and deliveries with no matching mailbox message', () => {
    const timeline = new LiveTimeline()
    timeline.apply({
      replace: true, hasMore: false,
      entries: [
        entry('timeline-entry/direct', 1, 'content', { media_type: 'text/plain', text: 'typed directly' }),
        entry('timeline-entry/unmatched', 2, 'content', {
          media_type: 'text/plain', text: '<smalltalk-message graph="message/unseen">unseen</smalltalk-message>',
        }),
        entry('timeline-entry/image', 3, 'content', { media_type: 'image/png', attachment_id: 'blob/image' }),
      ],
    })
    expect(timeline.project().items.map((item) => item.id)).toEqual([
      'timeline-entry/direct', 'timeline-entry/unmatched', 'timeline-entry/image',
    ])
  })
})
