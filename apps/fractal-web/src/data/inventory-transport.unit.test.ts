import type { ArrangementPage, EnvelopeOf } from '@smalltalk/st3-client'
import { describe, expect, it, vi } from 'vitest'
import { st3InventoryGateway } from './arrangements.ts'
import { testCapabilities, testSelection, testSnapshot } from '../folders/testGateway.ts'

interface PendingRequest {
  readonly limit: number
  readonly signal: AbortSignal | null | undefined
  readonly response: PromiseWithResolvers<Response>
}
const harness = async (order: 'editor-first' | 'follow-first') => {
  const editor = new AbortController()
  const follow = new AbortController()
  const requests: PendingRequest[] = []
  let discoveries = 0
  const envelope = <T>(value: T): EnvelopeOf<T> => ({ api_version: 'st3.client.v0', request_id: 'request/example', snapshot: testSnapshot, value })
  const gateway = st3InventoryGateway({
    baseUrl: 'https://gateway.example.test',
    fetchImpl: async (input, init) => {
      if (String(input).endsWith('/capabilities')) { discoveries++; return Response.json(envelope(testCapabilities)) }
      const response = Promise.withResolvers<Response>()
      const signal = init?.signal
      requests.push({ limit: Number(new URL(String(input)).searchParams.get('limit')), signal, response })
      if (signal?.aborted) response.reject(signal.reason)
      else signal?.addEventListener('abort', () => response.reject(signal.reason), { once: true })
      return response.promise
    },
  })
  await gateway.discover()
  const observe = (request: Promise<EnvelopeOf<ArrangementPage>>) => request.then(
    (value) => ({ _tag: 'Success' as const, value }),
    (cause: unknown) => ({ _tag: 'Failure' as const, cause }),
  )
  const readEditor = () => observe(gateway.arrangementsList(testSelection.owner, { limit: 50 }, editor.signal))
  const readFollow = () => observe(gateway.arrangementsList(testSelection.owner, { limit: 100 }, follow.signal))
  // Both calls start in one turn, before generated discovery/credential awaits reach fetch.
  const [editorResult, followResult] = order === 'editor-first'
    ? [readEditor(), readFollow()] : (() => { const following = readFollow(); return [readEditor(), following] })()
  await vi.waitFor(() => expect(requests).toHaveLength(2))
  return {
    editor, follow, editorResult: editorResult!, followResult: followResult!,
    editorRequest: requests.find((request) => request.limit === 50)!,
    followRequest: requests.find((request) => request.limit === 100)!,
    discoveries: () => discoveries,
    finishEditor: () => requests.find((request) => request.limit === 50)!.response.resolve(Response.json(envelope<ArrangementPage>({ kind: 'page', collection: 'arrangements', filters: { person: testSelection.owner }, items: [], page: { limit: 50, has_more: false, next_cursor: null, cursor_expires_at: null } }))),
    close: () => { editor.abort(); follow.abort() },
  }
}

describe('generated inventory transport cancellation ownership', () => {
  it.each(['editor-first', 'follow-first'] as const)('captures separate request signals for concurrent %s readers', async (order) => {
    const h = await harness(order)
    try {
      expect(h.editorRequest.signal).toBe(h.editor.signal)
      expect(h.followRequest.signal).toBe(h.follow.signal)
      expect(h.discoveries()).toBe(1)
    } finally { h.close() }
  })

  it.each(['editor-first', 'follow-first'] as const)('follow refresh cancels only its request for concurrent %s readers', async (order) => {
    const h = await harness(order)
    try {
      h.follow.abort(new DOMException('The subscription was refreshed.', 'AbortError'))
      expect(h.editorRequest.signal?.aborted).toBe(false)
      expect(h.followRequest.signal?.aborted).toBe(true)
      expect(await h.followResult).toMatchObject({ _tag: 'Failure', cause: { name: 'AbortError' } })
      h.finishEditor()
      expect(await h.editorResult).toMatchObject({ _tag: 'Success', value: { value: { items: [] } } })
      expect(h.editor.signal.aborted).toBe(false)
      expect(h.discoveries()).toBe(1)
    } finally { h.close() }
  })
})
