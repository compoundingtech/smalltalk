import { St3Client } from '@smalltalk/st3-client'
import * as Native from '@smalltalk/st3-client/schema'
import { Option } from 'effect'
import { describe, expect, it } from 'vitest'

import { gatewayContentSearch } from './contentSearchPort.ts'
import recording from './subjectReadPort.gateway.fixtures.json' with { type: 'json' }

const page = {
  kind: 'conversation-search',
  host_id: 'host/actual-member',
  indexed_at: '2026-10-08T09:10:11Z',
  incomplete_sources: ['native:remote-agent'],
  refreshing: true,
  items: [{
    agent_id: 'agent/actual', conversation_id: 'session/actual', entry_id: 'entry/actual',
    entry_type: 'tool-result', excerpt: 'Exact café & orchid excerpt, not a sidebar match.',
    timestamp: '2026-10-08T08:01:02Z',
  }],
  page: { limit: 2, has_more: true, next_cursor: 'opaque+/=', cursor_expires_at: '2026-10-08T10:00:00Z' },
}

const searchClient = ({
  state = 'granted',
  actor = 'person/operator',
  reply = () => Response.json({ api_version: 'st3.client.v0', snapshot: { id: 'snapshot/search' }, value: page }),
}: {
  readonly state?: string
  readonly actor?: string
  readonly reply?: () => Response
} = {}) => {
  const calls: URL[] = []
  const client = new St3Client({
    baseUrl: 'https://gateway.invalid',
    fetchImpl: async (input, init) => {
      const url = new URL(String(input))
      calls.push(url)
      expect(init?.method).toBe('GET')
      return url.pathname.endsWith('/capabilities')
        ? Response.json({ ...recording.capabilities, value: {
          ...recording.capabilities.value,
          session_actor: actor,
          capabilities: [{ id: 'read.projections', version: 0, state }],
        } })
        : reply()
    },
  })
  return { port: gatewayContentSearch(client), calls }
}

describe('native conversation content search port', () => {
  it('passes query filters and opaque cursor to the SDK and preserves every native page value', async () => {
    const { port, calls } = searchClient()
    const request = { text: 'café & orchid', agent: 'agent/actual', since: '2026-10-02T00:00:00Z', cursor: 'incoming+/=', limit: 2 }
    const result = await port.search(request)
    expect(calls.map((url) => url.pathname)).toEqual(['/v1/client/capabilities', '/v1/client/conversations/search'])
    const query = calls[1]?.searchParams
    for (const [key, value] of Object.entries(request)) expect(query?.get(key)).toBe(String(value))
    expect(result).toEqual({ _tag: 'Success', value: Native.decodeUnknownSync(Native.ConversationSearch)(page) })
    if (result._tag !== 'Success') return
    expect(result.value.items[0]?.excerpt).toBe(page.items[0]?.excerpt)
    expect(result.value.page.next_cursor).toEqual(Option.some('opaque+/='))
    expect(result.value.refreshing).toBe(true)
    expect(result.value.incomplete_sources).toEqual(['native:remote-agent'])
  })

  it.each(['ungranted', 'unsupported', 'unavailable'])('denies %s projection access without searching', async (state) => {
    const { port, calls } = searchClient({ state })
    expect(await port.search({ text: 'orchid' })).toMatchObject({ _tag: 'Refused', reason: 'ungranted' })
    expect(calls).toHaveLength(1)
  })

  it('does not claim search access for an agent actor', async () => {
    const { port, calls } = searchClient({ actor: 'agent/operator' })
    expect(await port.search({ text: 'orchid' })).toMatchObject({ _tag: 'Refused', reason: 'ungranted' })
    expect(calls).toHaveLength(1)
  })

  it('preserves cursor-gap refusal rather than inventing an empty result or restarting search', async () => {
    const error = {
      api_version: 'st3.client.v0', error_version: 'st3.client.error.v0',
      code: 'cursor-gap', message: 'Index changed; start again', retryable: false,
      request_id: 'request/search-gap', details: { cursor: 'real-cursor' },
    }
    const { port, calls } = searchClient({ reply: () => Response.json(error, { status: 409 }) })
    expect(await port.search({ text: 'orchid', cursor: 'real-cursor' })).toEqual({
      _tag: 'Refused', reason: 'failed', detail: error.message, error,
    })
    expect(calls).toHaveLength(2)
  })

  it('reports malformed native pages as failures, never loaded-empty', async () => {
    const { port } = searchClient({ reply: () => Response.json({ api_version: 'st3.client.v0', snapshot: {}, value: { items: [] } }) })
    expect(await port.search({ text: 'orchid' })).toMatchObject({ _tag: 'Refused', reason: 'failed' })
  })

  it('returns a real empty page with the original indexing and pagination metadata', async () => {
    const empty = { ...page, items: [], refreshing: false, page: { limit: 2, has_more: false, next_cursor: null, cursor_expires_at: null } }
    const { port } = searchClient({ reply: () => Response.json({ api_version: 'st3.client.v0', snapshot: {}, value: empty }) })
    expect(await port.search({ text: 'orchid' })).toEqual({ _tag: 'Success', value: Native.decodeUnknownSync(Native.ConversationSearch)(empty) })
  })
})
