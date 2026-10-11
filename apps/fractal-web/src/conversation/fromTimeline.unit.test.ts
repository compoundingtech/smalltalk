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

describe('native reasoning convention', () => {
  it.each([
    ['[reasoning]\nCompare the options.', 'assistant', 'Reasoning', 'Compare the options.'],
    ['[reasoning]\n', 'assistant', 'Reasoning', ''],
    ['[reasoning]', 'assistant', 'Text', '[reasoning]'],
    ['Answer contains [reasoning]\ninside it.', 'assistant', 'Text', 'Answer contains [reasoning]\ninside it.'],
    ['[reasoning]\nHuman quotation.', 'user', 'Text', '[reasoning]\nHuman quotation.'],
    ['[reasoning]\nSystem quotation.', 'system', 'Text', '[reasoning]\nSystem quotation.'],
  ])('projects %j (%s) as %s', (text, role, tag, expected) => {
    const timeline = new LiveTimeline()
    timeline.apply({
      replace: true, hasMore: false,
      entries: [Schema.decodeUnknownSync(TimelineEntry)({
        id: 'timeline-entry/thought', sequence: 1, revision: 1, final: false,
        timestamp: '2026-10-03T00:00:00Z', type: 'content', role,
        body: { media_type: 'text/plain', text },
      })],
    })
    const item = timeline.project().items[0]!
    expect(item).toMatchObject({ _tag: tag, text: expected, streaming: true })
    expect(item).not.toHaveProperty('durationMs')
  })
})

describe('native truncation boundary', () => {
  it('retains HasOlder but never fabricates a transcript notice', () => {
    const timeline = new LiveTimeline()
    timeline.apply({
      replace: true, hasMore: false,
      entries: [Schema.decodeUnknownSync(TimelineEntry)({
        id: 'timeline-entry/boundary', sequence: 1, revision: 1, final: true,
        timestamp: '2026-10-03T00:00:00Z', type: 'truncation', role: 'system',
        body: { reason: 'response-limit', omitted_from_sequence: 0, omitted_to_sequence: 20 },
      })],
    })
    expect(timeline.hasOlder).toBe(true)
    expect(timeline.project().items).toEqual([])
    timeline.apply({ replace: true, hasMore: false, entries: [] })
    expect(timeline.hasOlder).toBe(false)
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

  it.each([
    ['native', mailMessage, mailContent],
    ['stored fallback', 'timeline-entry/s1/111111111111111111111111', 'timeline-entry/s1/222222222222222222222222'],
  ])('retains an own-send id through %s echo, revisions and header paging, but not reload', (_, messageId, contentId) => {
    const timeline = new LiveTimeline()
    const header = entry(messageId, 4, 'message', {
      message_id: 'message/abc', from: 'person/operator', to: 'agent/example',
    })
    const content = entry(contentId, 5, 'content', { media_type: 'text/plain', text: 'Server prose' })
    timeline.keepOwnSendId(['message/abc'], 'pending/own-key')
    timeline.apply({ replace: true, hasMore: false, entries: [header] })
    // A header is correlation evidence, not a replacement for the visible optimistic prose.
    expect(timeline.project().items).toEqual([])
    timeline.apply({ replace: false, hasMore: false, entries: [content] })
    const echoed = timeline.project().items[0]
    expect(echoed).toMatchObject({ id: 'pending/own-key', text: 'Server prose' })
    timeline.apply({ replace: false, hasMore: false, entries: [] })
    expect(timeline.project().items[0]).toBe(echoed)
    timeline.apply({ replace: false, hasMore: false, entries: [{
      ...entry(contentId, 5, 'content', { media_type: 'text/plain', text: 'Revised server prose' }), revision: 2,
    }] })
    expect(timeline.project().items[0]).toMatchObject({ id: 'pending/own-key', text: 'Revised server prose' })
    timeline.apply({ replace: true, hasMore: true, entries: [content] })
    expect(timeline.project().items[0]).toMatchObject({ id: 'pending/own-key', text: 'Server prose' })
    timeline.apply({ replace: true, hasMore: false, entries: [header, content] })
    expect(timeline.project().items[0]?.id).toBe('pending/own-key')
    const reloaded = new LiveTimeline()
    reloaded.apply({ replace: true, hasMore: false, entries: [header, content] })
    expect(reloaded.project().items[0]?.id).toBe(contentId)
  })

  it('correlates by message identity, never by equal prose or adjacent native content', () => {
    const timeline = new LiveTimeline()
    timeline.keepOwnSendId(['message/abc'], 'pending/own-key')
    timeline.apply({
      replace: true, hasMore: false,
      entries: [
        ...shownMail(4),
        entry('timeline-entry/s1/3333333333333333-message', 8, 'message', {
          message_id: 'message/unrelated', from: 'person/operator', to: 'agent/example',
        }),
        entry('timeline-entry/s1/3333333333333333-content', 9, 'content', { media_type: 'text/plain', text: 'hello' }),
        entry('timeline-entry/native-10', 10, 'content', { media_type: 'text/plain', text: 'hello' }),
      ],
    })
    expect(timeline.project().items.map(item => item.id)).toEqual([
      'pending/own-key', 'timeline-entry/s1/3333333333333333-content', 'timeline-entry/native-10',
    ])
  })

  it('invalidates cached server ids when an acknowledgement supplies a new correlation identity', () => {
    const timeline = new LiveTimeline()
    timeline.apply({ replace: true, hasMore: false, entries: shownMail(4) })
    expect(timeline.project().items[0]?.id).toBe(mailContent)
    timeline.keepOwnSendId(['message/abc'], 'pending/own-key')
    expect(timeline.project().items[0]?.id).toBe('pending/own-key')
    const echoed = timeline.project().items[0]
    timeline.keepOwnSendId(['message/abc'], 'pending/own-key')
    expect(timeline.project().items[0]).toBe(echoed)
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

  it('keeps a stored-fallback page in its sequence order when observed timestamps disagree', () => {
    // The stored fallback sorts by sequence alone and may stamp entries from
    // `observed_at_unix_ms`: a result can carry an earlier timestamp than its call.
    const timeline = new LiveTimeline()
    timeline.apply({
      replace: true, hasMore: false,
      entries: [
        entry('timeline-entry/call', 4, 'tool_call', {
          call_id: 'call/1', name: 'shell', arguments: { command: 'true' },
        }, 'assistant', '2026-10-03T00:02:00Z'),
        entry('timeline-entry/result', 7, 'tool_result', {
          call_id: 'call/1', status: 'success', media_type: 'text/plain', content: 'ok',
        }, 'tool', '2026-10-03T00:01:00Z'),
      ],
    })
    const items = timeline.project().items
    expect(items.map((item) => item.id)).toEqual(['timeline-entry/call'])
    expect(items[0]).toMatchObject({ _tag: 'ToolCall', status: 'success' })
  })

  it('joins a delta result to its call when the result is stamped before it', () => {
    const timeline = new LiveTimeline()
    timeline.apply({
      replace: true, hasMore: false,
      entries: [entry('timeline-entry/call', 4, 'tool_call', {
        call_id: 'call/1', name: 'shell', arguments: { command: 'true' },
      }, 'assistant', '2026-10-03T00:02:00Z')],
    })
    timeline.apply({
      replace: false, hasMore: false,
      entries: [entry('timeline-entry/result', 7, 'tool_result', {
        call_id: 'call/1', status: 'success', media_type: 'text/plain', content: 'ok',
      }, 'tool', '2026-10-03T00:01:00Z')],
    })
    const items = timeline.project().items
    expect(items.map((item) => item.id)).toEqual(['timeline-entry/call'])
    expect(items[0]).toMatchObject({ _tag: 'ToolCall', status: 'success' })
  })

  it.each([false, true])('joins a result delivered before its call (separate frames: %s)', (separateFrames) => {
    const timeline = new LiveTimeline()
    timeline.apply({ replace: true, hasMore: false, entries: [] })
    timeline.project()
    const call = entry('timeline-entry/call', 4, 'tool_call', {
      call_id: 'call/1', name: 'shell', arguments: { command: 'true' },
    }, 'assistant', '2026-10-03T00:02:00Z')
    const result = entry('timeline-entry/result', 7, 'tool_result', {
      call_id: 'call/1', status: 'success', media_type: 'text/plain', content: 'ok',
    }, 'tool', '2026-10-03T00:01:00Z')
    if (separateFrames) {
      timeline.apply({ replace: false, hasMore: false, entries: [result] })
      expect(timeline.project().items).toMatchObject([{ callSeen: false }])
      timeline.apply({ replace: false, hasMore: false, entries: [call] })
    } else {
      // The stored fallback's delta poll sorts by timestamp: result(seq 7), call(seq 4).
      timeline.apply({ replace: false, hasMore: false, entries: [result, call] })
    }
    const projected = timeline.project()
    expect(projected.items.map((item) => item.id)).toEqual(['timeline-entry/call'])
    expect(projected.items[0]).toMatchObject({
      _tag: 'ToolCall', status: 'success', callSeen: true, result: { content: 'ok' },
    })
    expect(projected.changedFrom).toBe(0)
  })

  it.each([true, false])('joins 2000 reused-id pairs with a linear number of result visits (replace: %s)', (replace) => {
    let joins = 0
    const timeline = new LiveTimeline(() => { joins += 1 })
    const count = 2000
    const pairs = Array.from({ length: count }, (_, index) => [
      entry(`timeline-entry/call-${index}`, index * 4, 'tool_call', {
        call_id: 'native-call', name: 'shell', arguments: { command: 'true' },
      }, 'assistant'),
      entry(`timeline-entry/result-${index}`, index * 4 + 1, 'tool_result', {
        call_id: 'native-call', status: 'success', media_type: 'text/plain', content: `${index}`,
      }, 'tool'),
    ]).flat()
    timeline.apply({ replace: true, hasMore: false, entries: [] })
    timeline.project()
    joins = 0
    timeline.apply({ replace, hasMore: false, entries: pairs })
    const projected = timeline.project()
    expect(projected.items).toHaveLength(count)
    expect(projected.items.at(-1)).toMatchObject({
      id: `timeline-entry/call-${count - 1}`, status: 'success', result: { content: `${count - 1}` },
    })
    expect(joins).toBeGreaterThan(0)
    expect(joins).toBeLessThanOrEqual(4 * count)
    // A tail invocation cannot revisit or dirty the completed historical invocations.
    joins = 0
    timeline.apply({
      replace: false, hasMore: false,
      entries: [entry('timeline-entry/tail-call', count * 4, 'tool_call', {
        call_id: 'native-call', name: 'shell', arguments: { command: 'next' },
      }, 'assistant')],
    })
    const tail = timeline.project()
    expect(tail.changedFrom).toBe(count)
    expect(tail.items[0]).toBe(projected.items[0])
    expect(joins).toBe(0)
  })

  it.each([false, true])('batches reverse-arriving invocations without revisiting results (retained results: %s)', (retained) => {
    let joins = 0
    const timeline = new LiveTimeline(() => { joins += 1 })
    const count = 2000
    const calls = Array.from({ length: count }, (_, index) =>
      entry(`timeline-entry/call-${index}`, index * 4 + 4, 'tool_call', {
        call_id: 'native-call', name: 'shell', arguments: { command: 'true' },
      }, 'assistant'))
    const results = Array.from({ length: count }, (_, index) =>
      entry(`timeline-entry/result-${index}`, index * 4 + 5, 'tool_result', {
        call_id: 'native-call', status: 'success', media_type: 'text/plain', content: `${index}`,
      }, 'tool'))
    // One old invocation initially owns all retained results. Each added invocation must
    // split only its own final segment, even though they arrive in reverse sequence order.
    timeline.apply({
      replace: true, hasMore: false,
      entries: retained
        ? [entry('timeline-entry/old-call', 0, 'tool_call', {
            call_id: 'native-call', name: 'shell', arguments: { command: 'old' },
          }, 'assistant'), ...results.toReversed()]
        : [],
    })
    timeline.project()
    joins = 0
    timeline.apply({
      replace: false, hasMore: false,
      entries: retained ? calls.toReversed() : [...results.toReversed(), ...calls.toReversed()],
    })
    const items = timeline.project().items
    expect(items).toHaveLength(count + (retained ? 1 : 0))
    if (retained) {
      expect(items[0]).toMatchObject({ id: 'timeline-entry/old-call', status: 'running' })
      expect(items[0]).not.toHaveProperty('result')
    }
    expect(items.at(-1)).toMatchObject({
      id: 'timeline-entry/call-0', status: 'success', result: { content: '0' },
    })
    expect(joins).toBeGreaterThan(0)
    expect(joins).toBeLessThanOrEqual(4 * count)
  })

  it('visits only the result segment split by one late invocation', () => {
    let joins = 0
    const timeline = new LiveTimeline(() => { joins += 1 })
    const count = 2000
    const missing = 1000
    const frames = Array.from({ length: count }, (_, index) => {
      const call = entry(`timeline-entry/call-${index}`, index * 4, 'tool_call', {
        call_id: 'native-call', name: 'shell', arguments: { command: 'true' },
      }, 'assistant')
      const result = entry(`timeline-entry/result-${index}`, index * 4 + 1, 'tool_result', {
        call_id: 'native-call', status: 'success', media_type: 'text/plain', content: `${index}`,
      }, 'tool')
      return index === missing ? [result] : [call, result]
    }).flat()
    timeline.apply({ replace: true, hasMore: false, entries: frames })
    const before = timeline.project().items
    joins = 0
    timeline.apply({
      replace: false, hasMore: false,
      entries: [entry(`timeline-entry/call-${missing}`, missing * 4, 'tool_call', {
        call_id: 'native-call', name: 'shell', arguments: { command: 'true' },
      }, 'assistant')],
    })
    const after = timeline.project()
    expect(joins).toBe(1)
    expect(after.changedFrom).toBe(missing - 1)
    expect(after.items[0]).toBe(before[0])
    expect(after.items[missing]).toBe(before[missing])
    expect(after.items.at(-1)).toMatchObject({
      id: `timeline-entry/call-${missing}`, status: 'success', result: { content: `${missing}` },
    })
  })

  it('keeps invocation results separate when a call identity is reused', () => {
    const timeline = new LiveTimeline()
    const callA = entry('timeline-entry/call-a', 4, 'tool_call', {
      call_id: 'native-call', name: 'shell', arguments: { command: 'first' },
    }, 'assistant')
    const resultA = entry('timeline-entry/result-a', 7, 'tool_result', {
      call_id: 'native-call', status: 'success', media_type: 'text/plain', content: 'first result',
    }, 'tool')
    const callB = entry('timeline-entry/call-b', 8, 'tool_call', {
      call_id: 'native-call', name: 'shell', arguments: { command: 'second' },
    }, 'assistant')
    timeline.apply({ replace: true, hasMore: false, entries: [callA, resultA, callB] })
    const before = timeline.project().items
    expect(before).toMatchObject([
      { id: callA.id, status: 'success', result: { content: 'first result' } },
      { id: callB.id, status: 'running' },
    ])
    expect(before[1]).not.toHaveProperty('result')
    timeline.apply({
      replace: false, hasMore: false,
      entries: [entry('timeline-entry/result-b', 11, 'tool_result', {
        call_id: 'native-call', status: 'error', media_type: 'text/plain', content: 'second result',
      }, 'tool')],
    })
    const after = timeline.project().items
    expect(after).toMatchObject([
      { id: callA.id, status: 'success', result: { content: 'first result' } },
      { id: callB.id, status: 'error', result: { content: 'second result' } },
    ])
    expect(after[0]).toBe(before[0])
  })

  it.each([true, false])('segments reused identities by sequence when results arrive first (earlier call first: %s)', (earlierFirst) => {
    const timeline = new LiveTimeline()
    const resultA = entry('timeline-entry/result-a', 7, 'tool_result', {
      call_id: 'native-call', status: 'success', media_type: 'text/plain', content: 'first result',
    }, 'tool')
    const resultB = entry('timeline-entry/result-b', 11, 'tool_result', {
      call_id: 'native-call', status: 'error', media_type: 'text/plain', content: 'second result',
    }, 'tool')
    const callA = entry('timeline-entry/call-a', 4, 'tool_call', {
      call_id: 'native-call', name: 'shell', arguments: { command: 'first' },
    }, 'assistant')
    const callB = entry('timeline-entry/call-b', 8, 'tool_call', {
      call_id: 'native-call', name: 'shell', arguments: { command: 'second' },
    }, 'assistant')
    timeline.apply({ replace: true, hasMore: false, entries: [resultB, resultA] })
    timeline.project()
    timeline.apply({ replace: false, hasMore: false, entries: [earlierFirst ? callA : callB] })
    expect(timeline.project().items).toMatchObject(earlierFirst
      ? [{ id: callA.id, status: 'error', result: { content: 'second result' } }]
      : [
          { id: resultA.id, callSeen: false, result: { content: 'first result' } },
          { id: callB.id, status: 'error', result: { content: 'second result' } },
        ])
    // A later-arriving invocation redistributes results by sequence, not array position.
    timeline.apply({ replace: false, hasMore: false, entries: [earlierFirst ? callB : callA] })
    const expected = [
      { id: callA.id, status: 'success', result: { content: 'first result' } },
      { id: callB.id, status: 'error', result: { content: 'second result' } },
    ]
    expect(timeline.project().items).toMatchObject(earlierFirst ? expected : expected.toReversed())
  })

  it('removes every orphan row when its call arrives and joins the newest result by sequence', () => {
    const timeline = new LiveTimeline()
    const newer = entry('timeline-entry/newer', 8, 'tool_result', {
      call_id: 'call/1', status: 'success', media_type: 'text/plain', content: 'newer',
    }, 'tool')
    const older = entry('timeline-entry/older', 7, 'tool_result', {
      call_id: 'call/1', status: 'error', media_type: 'text/plain', content: 'older',
    }, 'tool')
    timeline.apply({ replace: true, hasMore: false, entries: [newer, older] })
    expect(timeline.project().items.map((item) => item.id)).toEqual([newer.id, older.id])
    timeline.apply({
      replace: false, hasMore: false,
      entries: [entry('timeline-entry/call', 4, 'tool_call', {
        call_id: 'call/1', name: 'shell', arguments: { command: 'true' },
      }, 'assistant')],
    })
    const projected = timeline.project()
    expect(projected.changedFrom).toBe(0)
    expect(projected.items).toMatchObject([
      { id: 'timeline-entry/call', callSeen: true, status: 'success', result: { content: 'newer' } },
    ])
    // A revision of the same result identity updates the call, without leaving an orphan.
    timeline.apply({
      replace: false, hasMore: false,
      entries: [Schema.decodeUnknownSync(TimelineEntry)({
        ...Schema.encodeSync(TimelineEntry)(newer), revision: 2, body: { ...newer.body, content: 'revised' },
      })],
    })
    expect(timeline.project().items).toMatchObject([
      { id: 'timeline-entry/call', callSeen: true, status: 'success', result: { content: 'revised' } },
    ])
    // An authoritative replacement cannot retain a result from the previous window.
    timeline.apply({
      replace: true, hasMore: false,
      entries: [entry('timeline-entry/call', 4, 'tool_call', {
        call_id: 'call/1', name: 'shell', arguments: { command: 'true' },
      }, 'assistant')],
    })
    const reset = timeline.project().items
    expect(reset).toMatchObject([{ id: 'timeline-entry/call', status: 'running' }])
    expect(reset[0]).not.toHaveProperty('result')
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
