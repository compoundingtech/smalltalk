import { describe, expect, it } from 'vitest'

import { nativeAgentFetch } from './nativeAgentFetch.ts'

const origin = 'https://edge.example'
const inlineCredentials = 'user:secret' + '@'
const probe = () => {
  const calls: Array<{ input: RequestInfo | URL; init?: RequestInit }> = []
  const response = new Response('{}')
  const fetch = nativeAgentFetch({ origin, fetchImpl: async (input, init) => {
    calls.push({ input, init }); return response
  } })
  return { fetch, calls, response }
}

describe('private native agent detail fetch boundary', () => {
  it('rewrites only the encoded canonical agent reference and preserves query and init by identity', async () => {
    const { fetch, calls, response } = probe()
    const headers = new Headers({ accept: 'application/json', 'x-st3-features': 'conversation-blocks.v1', 'x-st3-client': 'test' })
    const init: RequestInit = { method: 'GET', headers, credentials: 'same-origin', signal: new AbortController().signal }
    expect(await fetch(`${origin}/v1/client/agents/agent%2Fsetup%2Fexample?view=detail&cursor=a%2Fb`, init)).toBe(response)
    expect(calls).toHaveLength(1)
    expect(calls[0]?.input).toBe(`${origin}/v1/client/agents/agent/setup/example?view=detail&cursor=a%2Fb`)
    expect(calls[0]?.init).toBe(init)
    expect(calls[0]?.init?.headers).toBe(headers)
  })
  it('component-encodes safe Unicode segments without changing the canonical subject', async () => {
    const { fetch, calls } = probe()
    await fetch(`${origin}/v1/client/agents/${encodeURIComponent('agent/ada/prüfung')}`)
    expect(calls[0]?.input).toBe(`${origin}/v1/client/agents/agent/ada/pr%C3%BCfung`)
  })
  it('preserves Request headers, credentials, policy and abort signal when replacing its URL', async () => {
    const { fetch, calls } = probe()
    const controller = new AbortController()
    const request = new Request(`${origin}/v1/client/agents/agent%2Fada%2Fexample?view=detail`, {
      headers: { 'x-st3-client': 'test' }, credentials: 'include', cache: 'no-store', mode: 'same-origin', signal: controller.signal,
    })
    await fetch(request)
    const forwarded = calls[0]?.input
    expect(forwarded).toBeInstanceOf(Request)
    if (!(forwarded instanceof Request)) throw new Error('Expected Request')
    expect(forwarded.url).toBe(`${origin}/v1/client/agents/agent/ada/example?view=detail`)
    expect([...forwarded.headers]).toEqual([...request.headers])
    expect(forwarded.credentials).toBe(request.credentials)
    expect(forwarded.cache).toBe(request.cache)
    expect(forwarded.mode).toBe(request.mode)
    controller.abort()
    expect(forwarded.signal.aborted).toBe(true)
  })
  it('passes through foreign origins, non-GET methods and all routes outside the exact read by identity', async () => {
    for (const [url, method] of [
      ['https://foreign.example/v1/client/agents/agent%2Fada%2Fexample', 'GET'],
      [`${origin}/v1/client/agents/agent%2Fada%2Fexample`, 'POST'],
      [`${origin}/v1/client/agents/agent%2Fada%2Fexample`, 'HEAD'],
      [`${origin}/v1/client/actions`, 'POST'],
      [`${origin}/v1/client/conversations/agent%2Fada%2Fexample/changes`, 'GET'],
      [`${origin}/v1/client/sessions/example/timeline`, 'GET'],
      [`${origin}/v1/client/agents/agent/ada/example`, 'GET'],
      [`${origin}/v1/client/agents/plain`, 'GET'],
      [`${origin}/v1/client/agents/agent%2Fada%2Fexample/extra`, 'GET'],
    ]) {
      const { fetch, calls } = probe()
      const init = { method }
      await fetch(url!, init)
      expect(calls[0]?.input).toBe(url)
      expect(calls[0]?.init).toBe(init)
    }
  })
  it('uses init method override and never rewrites a POST Request', async () => {
    const { fetch, calls } = probe()
    const request = new Request(`${origin}/v1/client/agents/agent%2Fada%2Fexample`, { method: 'POST' })
    await fetch(request)
    expect(calls[0]?.input).toBe(request)
    await fetch(`${origin}/v1/client/agents/agent%2Fada%2Fexample`, { method: 'post' })
    expect(calls[1]?.input).toBe(`${origin}/v1/client/agents/agent%2Fada%2Fexample`)
  })
  it('rejects traversal, empty segments, double encoding, reserved characters and controls before fetch', async () => {
    for (const ref of [
      'agent/ada/../example', 'agent/./example', 'agent//example', 'agent/', 'person/ada',
      'agent/ada/%2Fexample', 'agent/ada/a?b', 'agent/ada/a#b', 'agent/ada/a\\b',
      'agent/ada/\u0000', 'agent/ada/\u001f', 'agent/ada/\u007f', 'agent/ada/\u200b',
    ]) {
      const { fetch, calls } = probe()
      await expect(fetch(`${origin}/v1/client/agents/${encodeURIComponent(ref)}`)).rejects.toThrow(TypeError)
      expect(calls).toHaveLength(0)
    }
    for (const segment of ['agent%252Fada%252Fexample', 'agent%2Fada%2Fbad%ZZ']) {
      const { fetch, calls } = probe()
      await expect(fetch(`${origin}/v1/client/agents/${segment}`)).rejects.toThrow(TypeError)
      expect(calls).toHaveLength(0)
    }
  })
  it('rejects credentials or fragments on an otherwise admitted route', async () => {
    for (const url of [`https://${inlineCredentials}edge.example/v1/client/agents/agent%2Fada%2Fexample`, `${origin}/v1/client/agents/agent%2Fada%2Fexample#ignored`]) {
      const { fetch, calls } = probe()
      await expect(fetch(url)).rejects.toThrow(TypeError)
      expect(calls).toHaveLength(0)
    }
  })
})
