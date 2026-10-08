import { it } from '@effect/vitest'
import { St3Client } from '@smalltalk/st3-client'
import * as Native from '@smalltalk/st3-client/schema'
import { Context, Effect, Layer, Schema } from 'effect'
import { describe, expect } from 'vitest'

import { SessionTraceProvider, TraceQuery, TraceSeries, TraceUnknown, sessionTrace } from './sessionTrace.ts'
import { StSessionTraceFacts, gatewaySessionTraceLayer, stSessionTraceLayer, type StTraceFacts } from './stSessionTrace.ts'
import { SubjectReadPort, gatewaySubjectReads } from './subjectReadPort.ts'
import recording from './subjectReadPort.gateway.fixtures.json' with { type: 'json' }

const query: TraceQuery = { native_session_id: 'native-example', range: '7d', bucket: '15m' }
const unknown = { _tag: 'Unknown', reason: 'not-observed' } as const
const summary = Native.decodeUnknownSync(Native.UsageSummary)({
  aggregation: 'cumulative-per-incarnation-else-response-deltas',
  incarnation_count: 2, input_tokens: 101, output_tokens: 21, cached_tokens: 7,
  total_tokens: 122, cost: 1.25, currency: 'USD',
  context: { observed_at_unix_ms: 1000, used_tokens: 12, window_tokens: 100, compactions: 2 },
})
const row = Native.decodeUnknownSync(Native.UsageRow)({
  agent: 'agent/example', native_session_id: query.native_session_id,
  input_tokens: 50, output_tokens: 10, cached_tokens: 5, cache_write_tokens: 2,
  cache_write_1h_tokens: 0, total_tokens: 60, cost_microusd: 100_000,
  reported_cost_microusd: 100_000, unpriced_tokens: 0,
})
const facts: StTraceFacts = {
  usage: { _tag: 'Known', value: summary }, row: { _tag: 'Known', value: row },
  freshness: { _tag: 'Observed', atMs: 1000 },
}
const nativeLayer = (observation: StTraceFacts) => stSessionTraceLayer.pipe(
  Layer.provide(Layer.succeed(StSessionTraceFacts, { read: () => Effect.succeed(observation) })),
)

/** Test-only independent provider; no native service is installed for this layer. */
const fakeProvider = (value: TraceSeries) => Layer.succeed(SessionTraceProvider, {
  load: Effect.fn('SessionTrace.test.load')((request: TraceQuery) => Effect.succeed({ ...value, query: request })),
})

class TraceConsumer extends Context.Service<TraceConsumer, { readonly trace: TraceSeries }>()('Test/TraceConsumer') {}
const consumerLayer = Layer.effect(TraceConsumer, Effect.gen(function* () {
  return { trace: yield* sessionTrace(query) }
}))

it.effect('context-only native summaries preserve context but do not invent zero spend', () =>
  Effect.gen(function* () {
    const contextOnly = { ...summary, incarnation_count: 0, input_tokens: 0, output_tokens: 0, cached_tokens: 0, total_tokens: 0 }
    const trace = yield* sessionTrace(query).pipe(Effect.provide(nativeLayer({
      ...facts, usage: { _tag: 'Known', value: contextOnly },
    })))
    for (const value of Object.values(trace.meters.tokens)) expect(value).toEqual(unknown)
    expect(trace.meters.cost).toEqual({ amount: unknown, currency: unknown })
    expect(trace.meters.context.usedTokens).toEqual({ _tag: 'Known', value: 12 })
    expect(yield* Schema.decodeUnknownEffect(TraceSeries)(trace)).toEqual(trace)
  }),
)

it.effect('valid empty native currency and model remain Unknown in the stricter public schema', () =>
  Effect.gen(function* () {
    const trace = yield* sessionTrace(query).pipe(Effect.provide(nativeLayer({
      ...facts, usage: { _tag: 'Known', value: { ...summary, currency: '', context: {
        observed_at_unix_ms: summary.context!.observed_at_unix_ms, model: '',
      } } },
    })))
    expect(trace.meters.cost.currency).toEqual(unknown)
    expect(trace.meters.context.model).toEqual(unknown)
    expect(yield* Schema.decodeUnknownEffect(TraceSeries)(trace)).toEqual(trace)
  }),
)

describe('session trace seam', () => {
  it.effect.prop('round trips the complete response schema', [TraceSeries], ([trace]) =>
    Effect.gen(function* () {
      const encoded = yield* Schema.encodeEffect(TraceSeries)(trace)
      expect(yield* Schema.decodeUnknownEffect(TraceSeries)(encoded)).toEqual(trace)
    }),
  )

  it.effect.prop('Unknown native inputs never turn into fabricated totals', [TraceUnknown], ([absence]) =>
    Effect.gen(function* () {
      const trace = yield* sessionTrace(query).pipe(Effect.provide(nativeLayer({
        usage: absence, row: absence, freshness: absence,
      })))
      expect(trace.native_session_id).toEqual(absence)
      expect(Object.values(trace.meters.tokens)).toEqual([absence, absence, absence, absence])
      expect(trace.meters.cost).toEqual({ amount: absence, currency: absence })
      for (const value of Object.values(trace.meters.context)) expect(value).toEqual(absence)
      expect(trace.buckets).toEqual({ _tag: 'Unknown', reason: 'no-provider' })
      expect(trace.subagents).toEqual({ _tag: 'Unknown', reason: 'no-provider' })
    }),
  )

  it.effect('labels native main-session meters partial and agent-incarnation scoped', () =>
    Effect.gen(function* () {
      const trace = yield* sessionTrace(query).pipe(Effect.provide(nativeLayer(facts)))
      expect(trace.scope).toBe('self')
      expect(trace.partial).toBe(true)
      expect(trace.meters.scope).toBe('agent-incarnations')
      expect(trace.meters.partial).toBe(true)
      expect(trace.meters.tokens.input).toEqual({ _tag: 'Known', value: 101 })
      expect(trace.meters.cost.amount).toEqual({ _tag: 'Known', value: 1.25 })
      expect(trace.native_session_id).toEqual({ _tag: 'Known', value: query.native_session_id })
      expect(trace.meters.context.usedTokens).toEqual({ _tag: 'Known', value: 12 })
      expect(trace.meters.context.observedAtMs).toEqual({ _tag: 'Known', value: 1000 })
      expect(trace.meters.context.lastCompactionMs).toEqual(unknown)
      expect(trace.meters.tokens.cacheWrite).toEqual(unknown)
      expect(trace.freshness).toEqual(facts.freshness)
    }),
  )

  it.effect('keeps absent optional counters and cost Unknown rather than zero', () =>
    Effect.gen(function* () {
      const { cost: _cost, currency: _currency, context: _context, ...withoutOptional } = summary
      const trace = yield* sessionTrace(query).pipe(Effect.provide(nativeLayer({
        ...facts, usage: { _tag: 'Known', value: withoutOptional },
      })))
      expect(trace.meters.cost).toEqual({ amount: unknown, currency: unknown })
      expect(trace.meters.tokens.cacheWrite).toEqual(unknown)
      for (const value of Object.values(trace.meters.context)) expect(value).toEqual(unknown)
    }),
  )

  it.effect('does not claim a Known join from a mismatching native row', () =>
    Effect.gen(function* () {
      const trace = yield* sessionTrace(query).pipe(Effect.provide(nativeLayer({
        ...facts, row: { _tag: 'Known', value: { ...row, native_session_id: 'other-native' } },
      })))
      expect(trace.native_session_id).toEqual({ _tag: 'Unknown', reason: 'not-attributed' })
    }),
  )

  it.effect('injects an independent series provider through Layer.provide without consumer changes', () =>
    Effect.gen(function* () {
      const native = yield* sessionTrace(query).pipe(Effect.provide(nativeLayer(facts)))
      const knownZero = { _tag: 'Known', value: 0 } as const
      const full: TraceSeries = {
        ...native, scope: 'including_subagents', partial: false,
        buckets: { _tag: 'Known', value: [{
          t: { _tag: 'Known', value: 1000 },
          tokens: { input: knownZero, output: knownZero, cacheRead: knownZero, cacheWrite: knownZero },
          cost: { amount: knownZero, currency: { _tag: 'Known', value: 'USD' } },
          model: { _tag: 'Known', value: 'example-model' },
        }] },
        subagents: { _tag: 'Known', value: [] },
      }
      const nativeConsumer = yield* Effect.gen(function* () { return (yield* TraceConsumer).trace }).pipe(
        Effect.provide(consumerLayer.pipe(Layer.provide(nativeLayer(facts)))),
      )
      const injectedConsumer = yield* Effect.gen(function* () { return (yield* TraceConsumer).trace }).pipe(
        Effect.provide(consumerLayer.pipe(Layer.provide(fakeProvider(full)))),
      )
      expect(nativeConsumer.buckets._tag).toBe('Unknown')
      expect(injectedConsumer).toEqual(full)
      expect(yield* Schema.decodeUnknownEffect(TraceSeries)(injectedConsumer)).toEqual(full)
    }),
  )
})

describe('native trace gateway adapter', () => {
  for (const mode of ['observed', 'missing', 'ambiguous', 'failed', 'invalid'] as const) {
    it.effect(`preserves native evidence for ${mode} usage`, () => Effect.gen(function* () {
      const requests: URL[] = []
      const fetchImpl: typeof fetch = async (input) => {
          const url = new URL(String(input))
          requests.push(url)
          if (url.pathname.endsWith('/capabilities')) return Response.json(recording.capabilities)
          if (url.pathname.endsWith('/usage')) {
            if (mode === 'failed') return new Response('Unavailable', { status: 503 })
            return Response.json({
              api_version: 'st3.client.v0', snapshot: recording.capabilities.snapshot,
              value: mode === 'invalid' ? { rows: 'invalid' } : {
                since_ms: 0, until_ms: 1000,
                rows: mode === 'missing' ? [] : mode === 'ambiguous'
                  ? [row, { ...row, agent: 'agent/other' }] : [row],
              },
            })
          }
          return Response.json({
            api_version: 'st3.client.v0', snapshot: recording.capabilities.snapshot,
            value: {
              id: 'agent/example', kind: 'agent', revision: '1',
              updated_at: '2026-10-08T00:00:00Z', name: 'Example',
              runtime_ids: [], state: 'running', reachability: 'reachable',
              usage: Schema.encodeSync(Native.UsageSummary)(summary),
            },
          })
      }
      const client = new St3Client({ baseUrl: 'https://gateway.invalid', fetchImpl })
      const subjects = gatewaySubjectReads({ options: { baseUrl: 'https://gateway.invalid', fetchImpl } })
      const trace = yield* sessionTrace(query).pipe(Effect.provide(
        gatewaySessionTraceLayer(client).pipe(Layer.provide(Layer.succeed(SubjectReadPort, subjects))),
      ))
      const usageRequest = requests.find((url) => url.pathname.endsWith('/usage'))
      expect(usageRequest).toBeDefined()
      expect(Number(usageRequest?.searchParams.get('until_ms')) - Number(usageRequest?.searchParams.get('since_ms'))).toBe(604_800_000)
      if (mode === 'observed') {
        expect(trace.meters.tokens.input).toEqual({ _tag: 'Known', value: 101 })
        expect(trace.native_session_id).toEqual({ _tag: 'Known', value: query.native_session_id })
        expect(requests.some((url) => url.pathname.includes('/agents/'))).toBe(true)
      } else {
        expect(trace.meters.tokens.input._tag).toBe('Unknown')
        expect(trace.native_session_id).toEqual({
          _tag: 'Unknown', reason: mode === 'missing' || mode === 'ambiguous' ? 'not-attributed' : 'failed',
          ...(mode === 'failed' || mode === 'invalid' ? { detail: 'Native usage is unavailable' } : {}),
        })
        expect(requests.some((url) => url.pathname.includes('/agents/'))).toBe(false)
      }
    }))
  }
})
