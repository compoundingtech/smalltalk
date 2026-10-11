import { describe, expect, it, vi } from 'vitest'
import type { ConversationItem, TextItem } from '../conversation/model.ts'
import { mergeOutboxItems } from './outbox.ts'

const row = (id: string, seconds: number): TextItem => ({
  _tag: 'Text', id, role: 'user', text: id, attachments: [], streaming: false,
  at: new Date(Date.UTC(2026, 9, 8, 12, 0) + seconds * 1000).toISOString(),
})

const ids = (items: readonly ConversationItem[]) => items.map(item => item.id)

describe('outbox send-time merge', () => {
  it('preserves transcript order and submission order with equal times and clock rollback', () => {
    const projected = [row('history', 0), row('later-before-earlier', 8), row('earlier', 2), row('reply', 9)]
    const pending = [row('first', 4), row('equal-time', 4), row('clock-rollback', 1), row('last', 8)].map(item => ({ item }))
    expect(ids(mergeOutboxItems(projected, pending))).toEqual([
      'history', 'later-before-earlier', 'earlier', 'first', 'equal-time', 'clock-rollback', 'last', 'reply',
    ])
  })

  it('retains untimed and unparseable transcript rows as stable insertion boundaries', () => {
    const untimed: ConversationItem = { _tag: 'UnknownEvent', id: 'untimed', eventType: 'custom', data: {} }
    const projected = [row('history', 0), { ...row('unparseable', 0), at: 'unparseable' }, untimed, row('reply', 9)]
    expect(ids(mergeOutboxItems(projected, [{ item: row('first', 1) }, { item: row('second', 2) }]))).toEqual([
      'history', 'unparseable', 'untimed', 'first', 'second', 'reply',
    ])
  })

  it('parses at most P + T timestamps for 1000 retained sends before 1000 later transcript rows', () => {
    const projected = Array.from({ length: 1000 }, (_, index) => row(`reply/${index}`, index + 10))
    const pending = Array.from({ length: 1000 }, (_, index) => ({ item: row(`pending/${index}`, 1) }))
    const parse = vi.spyOn(Date, 'parse')
    try {
      const merged = mergeOutboxItems(projected, pending)
      expect(parse.mock.calls.length).toBeLessThanOrEqual(projected.length + pending.length)
      expect(ids(merged)).toEqual([...pending.map(send => send.item.id), ...projected.map(item => item.id)])
    } finally {
      parse.mockRestore()
    }
  })
})
