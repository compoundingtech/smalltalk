import { describe, expect, it } from 'vitest'
import { instrumentFetch } from './transport.ts'

const parent = '00-0123456789abcdef0123456789abcdef-0123456789abcdef-01'
const child = '00-0123456789abcdef0123456789abcdef-abcdef0123456789-01'
const origin = 'https://example.test'
const harness = () => {
  const requests: Headers[] = []
  const fetchImpl: typeof fetch = async (input, init) => {
    requests.push(new Headers(init?.headers ?? (input instanceof Request ? input.headers : undefined)))
    return new Response(undefined, { status: 204 })
  }
  return { requests, fetch: instrumentFetch({ fetchImpl, origin, traceContext: () => ({ traceparent: parent, tracestate: 'vendor=value' }) }) }
}

describe('same-origin UX context propagation', () => {
  it('adds W3C context while preserving request headers and caller init', async () => {
    const h = harness()
    const init = { headers: { accept: 'application/json' } }
    await h.fetch(`${origin}/v1/client/agents`, init)
    expect(h.requests[0]?.get('traceparent')).toBe(parent)
    expect(h.requests[0]?.get('tracestate')).toBe('vendor=value')
    expect(h.requests[0]?.get('accept')).toBe('application/json')
    expect(init).toEqual({ headers: { accept: 'application/json' } })
  })
  it('keeps SDK child context instead of overwriting it with the UX root', async () => {
    const h = harness()
    await h.fetch(new Request(`${origin}/v1/client/capabilities`, { headers: { traceparent: child } }))
    expect(h.requests[0]?.get('traceparent')).toBe(child)
    expect(h.requests[0]?.get('tracestate')).toBeNull()
  })
  it('does not leak context across origins', async () => {
    const h = harness()
    await h.fetch('https://other.test/v1/client/agents')
    expect(h.requests[0]?.has('traceparent')).toBe(false)
    expect(h.requests[0]?.has('tracestate')).toBe(false)
  })
})
