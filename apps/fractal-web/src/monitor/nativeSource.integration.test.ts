import { createServer } from 'node:http'

import { it } from '@effect/vitest'
import { Effect } from 'effect'
import { describe, expect } from 'vitest'

import { nativeUsageSource } from './source.ts'
import { readQuota } from './state.ts'

const gateway = Effect.acquireRelease(
  Effect.promise(async () => {
    const requests: string[] = []
    let status = 200
    let limits: unknown = []
    const server = createServer((request, response) => {
      requests.push(request.url ?? '')
      response.writeHead(request.url === '/v1/client/usage' ? status : 404, { 'content-type': 'application/json' })
      response.end(JSON.stringify({ api_version: 'st3.client.v0', value: {
        since_ms: 0, until_ms: Date.now(), rows: [], ...(limits === undefined ? {} : { limits }),
      } }))
    })
    await new Promise<void>((resolve, reject) => {
      server.once('error', reject)
      server.listen(0, '127.0.0.1', resolve)
    })
    const address = server.address()
    if (address === null || typeof address === 'string') throw new Error('Missing loopback address')
    return { server, requests, source: nativeUsageSource({ label: 'Native st usage',
      prefix: `http://127.0.0.1:${address.port}/v1/client/usage` }),
      setStatus: (next: number) => { status = next },
      setLimits: (next: unknown) => { limits = next },
    }
  }),
  ({ server }) => Effect.promise(() => new Promise<void>((resolve) => server.close(() => resolve()))),
)

describe('canonical native quota source', () => {
  it.live('reads exactly the canonical usage endpoint and never invents a ledger history request', () => Effect.gen(function* () {
    const { source, requests } = yield* gateway
    expect(yield* readQuota(source)).toMatchObject({ _tag: 'Succeeded', envelope: {
      sourceSchema: 'st3.client.v0/usage.period', accounts: [],
    } })
    expect(yield* Effect.result(source.history('account/ada/claude-1'))).toMatchObject({
      _tag: 'Failure', failure: { kind: 'unavailable' },
    })
    expect(requests).toEqual(['/v1/client/usage'])
  }))
  it.live('keeps authorization/server errors distinct from unsupported or absent quota observations', () => Effect.gen(function* () {
    const { source, setStatus, setLimits } = yield* gateway
    for (const status of [401, 403, 500]) {
      setStatus(status)
      expect(yield* readQuota(source)).toMatchObject({ _tag: 'Failed', failure: { kind: 'http', reason: `quota returned HTTP ${status}` } })
    }
    setStatus(404)
    expect(yield* readQuota(source)).toMatchObject({ _tag: 'Failed', failure: { kind: 'unavailable' } })
    setStatus(200)
    setLimits(undefined)
    expect(yield* readQuota(source)).toMatchObject({ _tag: 'Failed', failure: { kind: 'unavailable' } })
    setLimits('malformed')
    expect(yield* readQuota(source)).toMatchObject({ _tag: 'Failed', failure: { kind: 'incompatible' } })
  }))
})
