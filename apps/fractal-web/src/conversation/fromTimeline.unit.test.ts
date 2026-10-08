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
  const entry = (
    id: string,
    sequence: number,
    type: string,
    body: unknown,
    role = 'user',
    timestamp = '2026-10-03T00:00:00Z',
  ) =>
    Schema.decodeUnknownSync(TimelineEntry)({
      id, sequence, type, body, role, revision: 1, final: true, timestamp,
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

describe('structural mailbox pairs and server ordering', () => {
  const entry = (
    id: string,
    sequence: number,
    type: string,
    body: unknown,
    role = 'user',
    timestamp = '2026-10-03T00:00:00Z',
  ) =>
    Schema.decodeUnknownSync(TimelineEntry)({
      id, sequence, type, body, role, revision: 1, final: true, timestamp,
    })

  it('identifies mailbox content by its pair structure across split pages and native interleaving', () => {
    const timeline = new LiveTimeline()
    const mailAt = '2026-10-03T00:00:00Z'
    const turnAt = '2026-10-03T00:01:00Z'
    timeline.apply({
      replace: true, hasMore: false,
      entries: [entry('timeline-entry/mail/message', 4, 'message', {
        message_id: 'message/abc', from: 'person/operator', to: 'agent/example',
      }, 'user', mailAt)],
    })
    timeline.apply({
      replace: false, hasMore: false,
      entries: [entry('timeline-entry/native/content', 5, 'content', {
        media_type: 'text/plain',
        text: `<smalltalk-message graph="message/abc">hello</smalltalk-message>
The person reads replies in st, not in the agent's session.`,
      }, 'user', turnAt)],
    })
    timeline.apply({
      replace: false, hasMore: false,
      entries: [entry('timeline-entry/mail/content', 5, 'content', {
        media_type: 'text/plain',
        text: `<smalltalk-message graph="message/abc">I quoted this envelope</smalltalk-message>
in my own reply`,
      }, 'user', mailAt)],
    })
    const items = timeline.project().items
    // st's merge order is (timestamp, sequence): the mail pair precedes the later harness turn.
    expect(items.map((item) => item.id)).toEqual(['timeline-entry/mail/message', 'timeline-entry/mail/content'])
    const mailContent = items.at(-1)
    expect(mailContent?._tag === 'Text' ? mailContent.text : '').toBe(
      `<smalltalk-message graph="message/abc">I quoted this envelope</smalltalk-message>
in my own reply`,
    )
  })

  it('orders merged entries by (timestamp, sequence) even when sequences disagree', () => {
    const timeline = new LiveTimeline()
    timeline.apply({
      replace: true, hasMore: false,
      entries: [
        entry('timeline-entry/later-turn', 2, 'content', {
          media_type: 'text/plain', text: 'later turn',
        }, 'assistant', '2026-10-03T00:02:00Z'),
        entry('timeline-entry/earlier-mail', 40, 'content', {
          media_type: 'text/plain', text: 'earlier mail',
        }, 'user', '2026-10-03T00:00:00Z'),
      ],
    })
    expect(timeline.project().items.map((item) => item.id))
      .toEqual(['timeline-entry/earlier-mail', 'timeline-entry/later-turn'])
  })

  it('treats content whose mailbox header never arrived as a native turn', () => {
    const timeline = new LiveTimeline()
    timeline.apply({
      replace: true, hasMore: false,
      entries: [
        entry('timeline-entry/shown/message', 1, 'message', {
          message_id: 'message/abc', from: 'person/operator', to: 'agent/example',
        }),
        entry('timeline-entry/shown/content', 2, 'content', { media_type: 'text/plain', text: 'hello' }),
        entry('timeline-entry/orphan/content', 3, 'content', {
          media_type: 'text/plain',
          text: `<smalltalk-message graph="message/abc">orphaned copy</smalltalk-message>
but this line is the harness's own`,
        }),
      ],
    })
    const items = timeline.project().items
    expect(items.map((item) => item.id)).toEqual([
      'timeline-entry/shown/message', 'timeline-entry/shown/content', 'timeline-entry/orphan/content',
    ])
    const orphan = items.at(-1)
    expect(orphan?._tag === 'Text' ? orphan.text : '').toBe(
      "but this line is the harness's own",
    )
  })
})
