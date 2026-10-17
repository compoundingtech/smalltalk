import { it } from '@effect/vitest'
import { St3Client } from '@smalltalk/st3-client'
import { Effect } from 'effect'
import * as Atom from 'effect/reactivity/Atom'
import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import { describe, expect } from 'vitest'
import recording from './subjectReadPort.gateway.fixtures.json' with { type: 'json' }
import { gatewayNativeDetails } from './nativeDetailsSource.ts'
import { gatewaySubjectReads } from './subjectReadPort.ts'
import { nativeDetailsTiming } from './nativeDetails.ts'
import type { Feed } from './source.ts'
import * as Native from '@smalltalk/st3-client/schema'

const nextFeed = <T>({ registry, atom, predicate }: { readonly registry: AtomRegistry.AtomRegistry; readonly atom: Atom.Atom<Feed<T>>; readonly predicate: (feed: Feed<T>) => boolean }) => Effect.acquireUseRelease(
  Effect.sync(() => {
    const { promise: ready, resolve } = Promise.withResolvers<Feed<T>>()
    const release = registry.subscribe(atom, value => { if (predicate(value)) resolve(value) }, { immediate: true })
    return { ready, release }
  }),
  value => Effect.promise(() => value.ready),
  value => Effect.sync(value.release),
)
const envelope = (value: unknown) => ({ ...recording.cases.agent.payload, value })
const usage = { since_ms: 1000, until_ms: 5000, rows: [{ agent: 'agent/example', native_session_id: 'native/root', input_tokens: 10, output_tokens: 20, cached_tokens: 0, cache_write_tokens: 0, cache_write_1h_tokens: 0, total_tokens: 30, cost_microusd: 1230000, reported_cost_microusd: 0, unpriced_tokens: 0 }] }
const declaration = { id: 'agent/example', revision: '1', revisions: ['1'], kdl: 'private raw text', tree: { name: 'agent', children: [{ name: 'workspace', arguments: ['/private/workspace'] }, { name: 'env', properties: { TOKEN: 'redacted' } }] } }

describe('real native Details getter adapters', () => {
  it.effect('shares period demand, decodes native wire, and retains failed observations as stale until retry', () => Effect.gen(function* () {
    const registry = yield* Effect.acquireRelease(Effect.sync(() => AtomRegistry.make()), registry => Effect.sync(() => registry.dispose()))
    const requests: URL[] = []
    let fail = false
    const options = { baseUrl: 'http://gateway.invalid', fetchImpl: async (input: RequestInfo | URL) => {
      requests.push(new URL(String(input)))
      if (String(input).endsWith('/capabilities')) return Response.json(recording.capabilities)
      if (fail) throw new TypeError('Connection lost')
      return Response.json(envelope(usage))
    } }
    const facts = gatewayNativeDetails({ options, discovery: new St3Client(options), registry, now: () => 604805000 })
    const mountA = registry.mount(facts.usagePeriod)
    const mountB = registry.mount(facts.usagePeriod)
    yield* Effect.addFinalizer(() => Effect.sync(() => { mountA(); mountB() }))
    const first = yield* nextFeed({ registry, atom: facts.usagePeriod, predicate: feed => feed._tag === 'Observed' && feed.freshness === 'live' })
    expect(first).toMatchObject({ _tag: 'Observed', value: { since_ms: 1000, until_ms: 5000 } })
    expect(requests.filter(url => url.pathname === '/v1/client/usage')).toHaveLength(1)
    expect(requests.find(url => url.pathname === '/v1/client/usage')?.searchParams.get('since_ms')).toBe('5000')
    expect(requests.some(url => url.pathname.includes('conversation'))).toBe(false)
    fail = true
    registry.refresh(facts.usagePeriod)
    const stale = yield* nextFeed({ registry, atom: facts.usagePeriod, predicate: feed => feed._tag === 'Observed' && feed.error?.reason === 'failed' })
    expect(stale).toMatchObject({ freshness: 'stale', value: { rows: [{ cost_microusd: 1230000 }] } })
    fail = false
    facts.retry(facts.usagePeriod)
    yield* nextFeed({ registry, atom: facts.usagePeriod, predicate: feed => feed._tag === 'Observed' && feed.freshness === 'live' })
    expect(requests.filter(url => url.pathname === '/v1/client/usage')).toHaveLength(3)
  }).pipe(Effect.scoped))

  it.effect('projects redacted declaration categories and refuses denial without retrying the forbidden read', () => Effect.gen(function* () {
    const registry = yield* Effect.acquireRelease(Effect.sync(() => AtomRegistry.make()), registry => Effect.sync(() => registry.dispose()))
    const requests: URL[] = []
    let deny = false
    const options = { baseUrl: 'http://gateway.invalid', fetchImpl: async (input: RequestInfo | URL) => {
      const url = new URL(String(input))
      requests.push(url)
      if (url.pathname.endsWith('/capabilities')) return Response.json(recording.capabilities)
      if (deny) return Response.json({ api_version: 'st3.client.v0', error_version: 'st3.client.error.v0', code: 'forbidden', message: 'read.declarations denied', retryable: false, request_id: 'request/denied', details: {} }, { status: 403 })
      return Response.json(envelope(declaration))
    } }
    const facts = gatewayNativeDetails({ options, discovery: new St3Client(options), registry, now: () => 5000 })
    const atom = facts.declarations({ agentRef: 'agent/example', revision: 'observation-one' })
    const release = registry.mount(atom)
    yield* Effect.addFinalizer(() => Effect.sync(release))
    const first = yield* nextFeed({ registry, atom, predicate: feed => feed._tag === 'Observed' })
    expect(first).toMatchObject({ value: { revision: '1', bindings: [{ kind: 'workspace', count: 1 }] } })
    expect(JSON.stringify(first)).not.toContain('/private/')
    expect(JSON.stringify(first)).not.toContain('TOKEN')
    const read = requests.find(url => url.pathname.includes('agent-declarations'))
    expect(read?.searchParams.get('show_env_values')).toBe('false')
    expect(read?.searchParams.has('revision')).toBe(false)
    deny = true
    // A revoked grant on the next poll; refresh the shared getter, not its public projection.
    registry.refresh(facts.rawDeclarations({ agentRef: 'agent/example', revision: 'observation-one' }))
    yield* nextFeed({ registry, atom, predicate: feed => feed._tag === 'Unavailable' && feed.reason === 'ungranted' })
    const count = requests.length
    facts.retry(atom)
    expect(requests).toHaveLength(count)
    expect(registry.get(atom)).toMatchObject({ _tag: 'Unavailable', reason: 'ungranted' })
  }).pipe(Effect.scoped))

  it.effect('serves raw and public declaration projections from one getter read', () => Effect.gen(function* () {
    const registry = yield* Effect.acquireRelease(Effect.sync(() => AtomRegistry.make()), registry => Effect.sync(() => registry.dispose()))
    const reads: URL[] = []
    const options = { baseUrl: 'http://gateway.invalid', fetchImpl: async (input: RequestInfo | URL) => {
      const url = new URL(String(input))
      if (url.pathname.endsWith('/capabilities')) return Response.json(recording.capabilities)
      reads.push(url)
      return Response.json(envelope(declaration))
    } }
    const facts = gatewayNativeDetails({ options, discovery: new St3Client(options), registry, now: () => 5000 })
    const query = { agentRef: 'agent/example', revision: '1' }
    const releasePublic = registry.mount(facts.declarations(query))
    const releaseRaw = registry.mount(facts.rawDeclarations(query))
    yield* Effect.addFinalizer(() => Effect.sync(() => { releasePublic(); releaseRaw() }))
    const raw = yield* nextFeed({ registry, atom: facts.rawDeclarations(query), predicate: feed => feed._tag === 'Observed' && feed.freshness === 'live' })
    const projected = yield* nextFeed({ registry, atom: facts.declarations(query), predicate: feed => feed._tag === 'Observed' && feed.freshness === 'live' })
    expect(raw).toMatchObject({ value: { id: 'agent/example', revision: '1' } })
    expect(projected).toMatchObject({ value: { bindings: [{ kind: 'workspace', count: 1 }] } })
    expect(JSON.stringify(projected)).not.toContain('/private/')
    expect(reads).toHaveLength(1)
    expect(reads[0]?.searchParams.get('show_env_values')).toBe('false')
  }).pipe(Effect.scoped))

  it.effect('reuses the real session SubjectReader and keeps elapsed independent of root availability', () => Effect.gen(function* () {
    const registry = yield* Effect.acquireRelease(Effect.sync(() => AtomRegistry.make()), registry => Effect.sync(() => registry.dispose()))
    const sessionValue = { id: 'session/current', kind: 'session', owner_id: 'agent/example', runtime_incarnation: 'inc/current', started_at: '1970-01-01T00:00:01Z', ended_at: '1970-01-01T00:00:03Z', updated_at: '1970-01-01T00:00:03Z', revision: '1', state: 'completed', timeline_cursor: 'cursor/0' }
    const options = { baseUrl: 'http://gateway.invalid', fetchImpl: async (input: RequestInfo | URL) => Response.json(String(input).endsWith('/capabilities') ? recording.capabilities : envelope(sessionValue)) }
    const reads = gatewaySubjectReads({ options, registry })
    const session = yield* reads.session.read('session/current')
    const row = Native.decodeUnknownSync(Native.Agent)({ id: 'agent/example', kind: 'agent', name: 'Example', runtime_ids: [], incarnation_id: 'inc/current', current_session_id: 'session/current', revision: '1', updated_at: '1970-01-01T00:00:04Z', state: 'running', reachability: 'local' })
    expect(nativeDetailsTiming({ row, session, now: 10000 })).toEqual({ elapsedMs: 2000, sessionAgeMs: 9000 })
  }).pipe(Effect.scoped))

  it.effect('cancels in-flight getter transport when final pane demand is released', () => Effect.gen(function* () {
    const registry = yield* Effect.acquireRelease(Effect.sync(() => AtomRegistry.make()), registry => Effect.sync(() => registry.dispose()))
    const requests: URL[] = []
    const { promise: arrival, resolve: started } = Promise.withResolvers<AbortSignal>()
    const { promise: cancellation, resolve: cancelled } = Promise.withResolvers<void>()
    const options = { baseUrl: 'http://gateway.invalid', fetchImpl: async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
      const url = new URL(String(input))
      requests.push(url)
      // Shared discovery is not pane-owned transport; admit the selected getter first.
      if (url.pathname.endsWith('/capabilities')) return Response.json(recording.capabilities)
      expect(url.pathname).toBe('/v1/client/usage')
      const signal = init?.signal
      if (signal === undefined || signal === null) throw new TypeError('Native getter must carry a cancellation signal')
      const { promise, reject } = Promise.withResolvers<Response>()
      signal.addEventListener('abort', () => { cancelled(); reject(new DOMException('Aborted', 'AbortError')) }, { once: true })
      started(signal)
      return promise
    } }
    const facts = gatewayNativeDetails({ options, discovery: new St3Client(options), registry, now: () => 5000 })
    const releaseA = registry.mount(facts.usagePeriod)
    const releaseB = registry.mount(facts.usagePeriod)
    yield* Effect.addFinalizer(() => Effect.sync(() => { releaseA(); releaseB() }))
    const signal = yield* Effect.promise(() => arrival)
    expect(requests.filter(url => url.pathname === '/v1/client/usage')).toHaveLength(1)
    releaseA()
    expect(signal.aborted).toBe(false)
    releaseB()
    yield* Effect.promise(() => cancellation)
    expect(signal.aborted).toBe(true)
  }).pipe(Effect.scoped))
})
