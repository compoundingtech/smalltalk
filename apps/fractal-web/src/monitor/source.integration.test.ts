import { createServer } from 'node:http'

import { it } from '@effect/vitest'
import { Effect } from 'effect'
import { describe, expect } from 'vitest'

import { FIXTURE_NOW, quotaEnvelope } from './fixtures.ts'
import { dependencyNote, initialQuota, reduceQuota } from './model.ts'
import { readQuota } from './state.ts'
import { relayUsageSource } from './source.ts'
import { decodeQuota } from './wire.ts'

/** Exercise the real relay reader over loopback HTTP, without replacing global fetch. */
const gateway = Effect.acquireRelease(
  Effect.promise(async () => {
    let status = 200
    let body: unknown = quotaEnvelope()
    const requests: Array<string> = []
    const server = createServer((request, response) => {
      requests.push(request.url ?? '')
      response.writeHead(status, { 'content-type': 'application/json' })
      response.end(status === 200 ? JSON.stringify(body) : 'unsupported')
    })
    await new Promise<void>((resolve, reject) => {
      server.once('error', reject)
      server.listen(0, '127.0.0.1', resolve)
    })
    const address = server.address()
    if (address === null || typeof address === 'string') throw new Error('Missing loopback address')
    return {
      server,
      requests,
      source: relayUsageSource({
        label: 'Gateway usage',
        prefix: `http://127.0.0.1:${address.port}/v1/client/usage`,
      }),
      setStatus: (next: number) => {
        status = next
      },
      setBody: (next: unknown) => {
        body = next
      },
    }
  }),
  ({ server }) =>
    Effect.promise(() => new Promise<void>((resolve) => server.close(() => resolve()))),
)

describe('usage relay availability', () => {
  it.live('reads the quota relay path and preserves producer timestamps and account facts', () =>
    Effect.gen(function* () {
      const { source, requests, setBody } = yield* gateway
      const payload = quotaEnvelope({ generatedAgo: 123_456 })
      setBody(payload)
      const envelope = yield* source.quota
      const decoded = decodeQuota(envelope)
      expect(decoded._tag).toBe('ok')
      if (decoded._tag !== 'ok') return

      const producerAccount = payload.accounts.find((account) => account.ledgerAccountId === 'anthropic/alpha')
      const decodedAccount = decoded.value.accounts.find((account) => account.ledgerAccountId === 'anthropic/alpha')
      expect(requests).toEqual(['/v1/client/usage/quota'])
      expect(decoded.value.generatedAt).toBe(payload.generatedAt)
      expect(decoded.value.freshnessHorizonSeconds).toBe(payload.freshnessHorizonSeconds)
      expect(decodedAccount).toEqual(producerAccount)
      expect(decodedAccount?.valueObservedAt).toBe(producerAccount?.valueObservedAt)
      expect(decodedAccount?.resetCredits).toEqual(producerAccount?.resetCredits)
    }),
  )

  it.live('rejects incompatible producer versions instead of projecting a native usage period', () =>
    Effect.gen(function* () {
      const { source, setBody } = yield* gateway
      setBody({ schemaVersion: 2, generatedAt: FIXTURE_NOW })
      const decoded = decodeQuota(yield* source.quota)
      expect(decoded).toMatchObject({
        _tag: 'incompatible',
        reason: 'incompatible usage hub quota schema version 2 (expected 3)',
      })
      expect(yield* readQuota(source)).toMatchObject({
        _tag: 'Failed',
        failure: { kind: 'incompatible' },
      })
    }),
  )

  it.live('treats only quota 404 as unavailable, leaving other HTTP failures intact', () =>
    Effect.gen(function* () {
      const { source, setStatus } = yield* gateway
      setStatus(404)
      const quota = yield* Effect.result(source.quota)
      expect(quota._tag).toBe('Failure')
      if (quota._tag !== 'Failure') return
      const state = reduceQuota({
        state: initialQuota(true),
        event: { _tag: 'Failed', at: FIXTURE_NOW, failure: quota.failure },
      })
      expect(dependencyNote({ state, now: FIXTURE_NOW })).toEqual({
        _tag: 'unavailable',
        generatedAt: undefined,
      })
      expect(yield* Effect.result(source.history('claude/alpha'))).toMatchObject({
        _tag: 'Failure',
        failure: { kind: 'http', reason: 'history returned HTTP 404' },
      })
      for (const status of [401, 403, 500]) {
        setStatus(status)
        expect(yield* Effect.result(source.quota)).toMatchObject({
          _tag: 'Failure',
          failure: { kind: 'http', reason: `quota returned HTTP ${status}` },
        })
      }
    }),
  )

  it.live('retains reported data through unavailable and failed refreshes, then recovers', () =>
    Effect.gen(function* () {
      const { source, setStatus } = yield* gateway
      const decoded = decodeQuota(yield* source.quota)
      expect(decoded._tag).toBe('ok')
      if (decoded._tag !== 'ok') return
      let state = reduceQuota({
        state: initialQuota(true),
        event: { _tag: 'Succeeded', at: FIXTURE_NOW, envelope: decoded.value },
      })
      if (state._tag !== 'declared') throw new Error('Declared source lost')
      const snapshot = state.last
      for (const status of [404, 500]) {
        setStatus(status)
        const read = yield* Effect.result(source.quota)
        if (read._tag !== 'Failure') throw new Error('Expected HTTP failure')
        state = reduceQuota({
          state,
          event: { _tag: 'Failed', at: FIXTURE_NOW + 1, failure: read.failure },
        })
        if (state._tag !== 'declared') throw new Error('Declared source lost')
        expect(state.last).toBe(snapshot)
        expect(dependencyNote({ state, now: FIXTURE_NOW })).toMatchObject({
          _tag: status === 404 ? 'unavailable' : 'retained',
          generatedAt: decoded.value.generatedAt,
        })
      }
      setStatus(200)
      const recovered = decodeQuota(yield* source.quota)
      if (recovered._tag !== 'ok') throw new Error(recovered.reason)
      state = reduceQuota({
        state,
        event: { _tag: 'Succeeded', at: FIXTURE_NOW + 2, envelope: recovered.value },
      })
      expect(dependencyNote({ state, now: FIXTURE_NOW })).toEqual({
        _tag: 'current',
        generatedAt: recovered.value.generatedAt,
      })
    }),
  )
})
