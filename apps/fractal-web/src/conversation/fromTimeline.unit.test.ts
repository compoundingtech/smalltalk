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
        entry('timeline-entry/s1/1111111111111111-message', 3, 'message', {
          message_id: 'message/quote', from: 'person/operator', to: 'agent/example',
        }),
        entry('timeline-entry/s1/1111111111111111-content', 4, 'content', {
          media_type: 'text/plain',
          text: `<smalltalk-message graph="message/abc">I received this envelope</smalltalk-message>
and copied it into my reply`,
        }),
      ],
    })
    expect(timeline.project().items.map((item) => item.id)).toEqual([
      'timeline-entry/first/message', 'timeline-entry/first/content',
      'timeline-entry/s1/1111111111111111-content',
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
        entry('timeline-entry/s1/2222222222222222-message', 3, 'message', {
          message_id: 'message/only', from: 'person/operator', to: 'agent/example',
        }),
        entry('timeline-entry/s1/2222222222222222-content', 4, 'content', {
          media_type: 'text/plain',
          text: '<smalltalk-message graph="message/abc">the quoted mail</smalltalk-message>',
        }),
      ],
    })
    expect(timeline.project().items.map((item) => item.id)).toEqual([
      'timeline-entry/first/message', 'timeline-entry/first/content',
      'timeline-entry/s1/2222222222222222-content',
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

describe('mailbox provenance and server ordering', () => {
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

  // st mints mailbox entry ids from the message claim (`client_v0.rs`): the native projection
  // pairs `<leaf>/<digest16>-message` with `-content`; the stored fallback mints `<leaf>/<digest24>`.
  const mailMessage = 'timeline-entry/s1/0123456789abcdef-message'
  const mailContent = 'timeline-entry/s1/0123456789abcdef-content'
  const shownMail = (sequence: number, timestamp = '2026-10-03T00:00:00Z') => [
    entry(mailMessage, sequence, 'message', {
      message_id: 'message/abc', from: 'person/operator', to: 'agent/example',
    }, 'user', timestamp),
    entry(mailContent, sequence + 1, 'content', { media_type: 'text/plain', text: 'hello' }, 'user', timestamp),
  ]
  const quote = `<smalltalk-message graph="message/abc">I quoted this envelope</smalltalk-message>
in my own words`

  it.each([
    ['native', mailMessage, mailContent],
    ['stored fallback', 'timeline-entry/s1/111111111111111111111111', 'timeline-entry/s1/222222222222222222222222'],
  ])('joins a %s person header to its content without hiding separate agent mail', (_, messageId, contentId) => {
    const timeline = new LiveTimeline()
    timeline.apply({
      replace: true, hasMore: false,
      entries: [
        entry(messageId, 4, 'message', { message_id: 'message/abc', from: 'person/operator', to: 'agent/example' }),
        entry(contentId, 5, 'content', { media_type: 'text/plain', text: 'hello' }),
        entry('timeline-entry/s1/3333333333333333-message', 8, 'message', { message_id: 'message/separate', from: 'agent/other', to: 'agent/example' }),
        entry('timeline-entry/s1/3333333333333333-content', 9, 'content', { media_type: 'text/plain', text: 'Separate agent mail' }),
      ],
    })
    expect(timeline.project().items.map(item => item.id)).toEqual([
      contentId, 'timeline-entry/s1/3333333333333333-message', 'timeline-entry/s1/3333333333333333-content',
    ])
    expect(Array.from(timeline.shownMessageIds())).toEqual(['message/abc', 'message/separate'])
  })

  it('keeps a person header until its own content arrives, then removes the extra row across deltas', () => {
    const timeline = new LiveTimeline()
    timeline.apply({ replace: true, hasMore: false, entries: [shownMail(4)[0]!] })
    expect(timeline.project().items.map(item => item.id)).toEqual([mailMessage])
    timeline.apply({
      replace: false, hasMore: false,
      entries: [entry('timeline-entry/native-5', 5, 'content', { media_type: 'text/plain', text: 'Unrelated native text' })],
    })
    expect(timeline.project().items.map(item => item.id)).toEqual([mailMessage, 'timeline-entry/native-5'])
    timeline.apply({ replace: false, hasMore: false, entries: [shownMail(4)[1]!] })
    expect(timeline.project().items.map(item => item.id)).toEqual(['timeline-entry/native-5', mailContent])
    timeline.apply({ replace: true, hasMore: true, entries: [shownMail(4)[0]!] })
    expect(timeline.project().items.map(item => item.id)).toEqual([mailMessage])
  })

  it('identifies mailbox content by its minted id across split pages and native interleaving', () => {
    const timeline = new LiveTimeline()
    const mailAt = '2026-10-03T00:00:00Z'
    const turnAt = '2026-10-03T00:01:00Z'
    timeline.apply({
      replace: true, hasMore: false,
      entries: [entry(mailMessage, 4, 'message', {
        message_id: 'message/abc', from: 'person/operator', to: 'agent/example',
      }, 'user', mailAt)],
    })
    timeline.apply({
      replace: false, hasMore: false,
      entries: [entry('timeline-entry/native-5', 5, 'content', {
        media_type: 'text/plain',
        text: `<smalltalk-message graph="message/abc">hello</smalltalk-message>
The person reads replies in st, not in the agent's session.`,
      }, 'user', turnAt)],
    })
    timeline.apply({
      replace: false, hasMore: false,
      entries: [entry(mailContent, 5, 'content', { media_type: 'text/plain', text: quote }, 'user', mailAt)],
    })
    const items = timeline.project().items
    expect(items.map((item) => item.id)).toEqual([mailContent])
    const kept = items.at(-1)
    expect(kept?._tag === 'Text' ? kept.text : '').toBe(quote)
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

  it('strips a shown delivery from a native turn and keeps the turn\'s own text', () => {
    const timeline = new LiveTimeline()
    timeline.apply({
      replace: true, hasMore: false,
      entries: [
        entry('timeline-entry/shown/message', 1, 'message', {
          message_id: 'message/abc', from: 'person/operator', to: 'agent/example',
        }),
        entry('timeline-entry/shown/content', 2, 'content', { media_type: 'text/plain', text: 'hello' }),
        entry('timeline-entry/native-3', 3, 'content', {
          media_type: 'text/plain',
          text: `<smalltalk-message graph="message/abc">orphaned copy</smalltalk-message>
but this line is the harness's own`,
        }),
      ],
    })
    const items = timeline.project().items
    expect(items.map((item) => item.id)).toEqual([
      'timeline-entry/shown/message', 'timeline-entry/shown/content', 'timeline-entry/native-3',
    ])
    const orphan = items.at(-1)
    expect(orphan?._tag === 'Text' ? orphan.text : '').toBe(
      "but this line is the harness's own",
    )
  })

  it.each([
    ['native projection', 'timeline-entry/s1/fedcba9876543210-content'],
    ['stored fallback', 'timeline-entry/s1/fedcba9876543210fedcba98'],
  ])('keeps %s mailbox content verbatim when its header is outside the window', (_, id) => {
    const timeline = new LiveTimeline()
    timeline.apply({
      replace: true, hasMore: true,
      entries: [...shownMail(0), entry(id, 9, 'content', { media_type: 'text/plain', text: quote })],
    })
    const items = timeline.project().items
    expect(items.map((item) => item.id)).toEqual([mailContent, id])
    const kept = items.at(-1)
    expect(kept?._tag === 'Text' ? kept.text : '').toBe(quote)
  })

  it('suppresses a native delivery whose sequence lands in a mailbox content slot', () => {
    // Native content at (line+1)*16+1 = 17 and a mail header at store_index*4 = 16 share a
    // millisecond: slot structure alone would read the native copy as the mail's own content.
    const timeline = new LiveTimeline()
    timeline.apply({
      replace: true, hasMore: false,
      entries: [
        entry(mailMessage, 16, 'message', {
          message_id: 'message/abc', from: 'person/operator', to: 'agent/example',
        }),
        entry('timeline-entry/native-17', 17, 'content', {
          media_type: 'text/plain',
          text: '<smalltalk-message graph="message/abc">hello</smalltalk-message>',
        }),
        entry(mailContent, 17, 'content', { media_type: 'text/plain', text: 'hello' }),
      ],
    })
    expect(timeline.project().items.map((item) => item.id)).toEqual([mailContent])
  })
})
