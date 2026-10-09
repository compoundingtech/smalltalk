import { it } from '@effect/vitest'
import type {
  Agent,
  CollectionCommand,
  CollectionFrame,
  CollectionSocket,
  Runtime,
  Snapshot,
  TerminalScreen,
} from '@smalltalk/st3-client'
import type { AttachmentSendRequest, MessageSendAction } from './source.ts'
import { Effect, Layer } from 'effect'
import * as Option from 'effect/Option'
import * as Atom from 'effect/reactivity/Atom'
import { afterEach, beforeEach, describe, expect, vi } from 'vitest'

/** Cold-selection admission through the real live source and SDK. */

import { liveSource, type LiveSource } from './liveSource.ts'
import { SessionTraceProvider } from './sessionTrace.ts'

const snapshot: Snapshot = {
  id: 'snapshot/1',
  created_at: '2026-10-03T00:00:00Z',
  host_id: 'host/example',
  projection_version: 'client-projection.v0',
  store_index: 1,
}

const agent: Agent = {
  id: 'agent/example',
  kind: 'agent',
  revision: '1',
  updated_at: snapshot.created_at,
  name: 'Example',
  state: 'running',
  reachability: 'local',
  runtime_ids: ['runtime/old'],
}

/** Socket commands and HTTP attachments seen by the gateway, not a mocked SDK service. */
class Gateway {
  socket: CollectionSocket | undefined
  readonly commands: CollectionCommand[] = []
  readonly sentFollowSubscribes: Array<{ readonly id: string; readonly key: string }> = []
  messageGrant: 'granted' | 'ungranted' = 'granted'
  transportFailure = false
  capabilityGate: Promise<void> | undefined
  rejectedCredential = false
  runtimeRefusalStatus: number | undefined
  sendGate: Promise<void> | undefined
  rejectSend = false
  currentSnapshot: Snapshot | undefined = snapshot
  staleSendFence = false
  capabilityReads = 0
  readonly messageActions: MessageSendAction[] = []
  /** Resolves once the gateway has derived the send action's message identity. */
  readonly nextAction = (count = 1) =>
    this.readyActions.has(count)
      ? Promise.resolve()
      : new Promise<void>((arrived) => this.actionArrivals.push({ count, arrived }))
  private readonly readyActions = new Set<number>()
  private readonly actionArrivals: Array<{ readonly count: number; readonly arrived: () => void }> = []
  echoMessageId = ''

  readonly fetch: typeof fetch = async (input, init) => {
    const path = new URL(String(input)).pathname
    let value: unknown
    if (path === '/v1/client/capabilities') {
      this.capabilityReads += 1
      await this.capabilityGate
      if (this.rejectedCredential)
        return new Response(
          JSON.stringify({
            api_version: 'st3.client.v0',
            error_version: 'st3.client.error.v0',
            code: 'forbidden',
            message: 'Device credential revoked',
            retryable: false,
            request_id: 'request/revoked',
            details: {},
          }),
          { status: 403, headers: { 'content-type': 'application/json' } },
        )
      value = {
        kind: 'capabilities',
        capabilities: [
          { id: 'work.done', state: 'granted', version: 0 },
          { id: 'message.send', state: this.messageGrant, version: 0 },
        ],
        event_cursor: 'cursor/current',
        oldest_event_cursor: 'cursor/oldest',
        limits: {
          max_page_items: 100,
          max_event_items: 100,
          max_wait_ms: 1000,
          max_response_bytes: 65536,
        },
        schemas: ['client-v0'],
        session_actor: 'person/operator',
        transport: 'fabric-loopback',
      }
    } else if (path.startsWith('/v1/client/runtimes/')) {
      if (this.runtimeRefusalStatus !== undefined)
        return new Response(JSON.stringify({
          api_version: 'st3.client.v0', error_version: 'st3.client.error.v0',
          code: this.runtimeRefusalStatus === 503 ? 'unavailable' : 'forbidden',
          message: 'Runtime read refused', retryable: false, request_id: 'request/runtime', details: {},
        }), { status: this.runtimeRefusalStatus, headers: { 'content-type': 'application/json' } })
      const name = decodeURIComponent(path.slice('/v1/client/runtimes/'.length)).slice(
        'runtime/'.length,
      )
      value = {
        id: `runtime/${name}`,
        kind: 'runtime',
        revision: '1',
        updated_at: snapshot.created_at,
        desired_revision: null,
        incarnation_id: `incarnation-${name}`,
        owner_host_id: 'host/example',
        owner_id: agent.id,
        runtime_id: name,
        runtime_kind: 'agent',
        state: 'running',
        terminal_id: `terminal/${name}`,
      } satisfies Runtime
    } else if (path === '/v1/client/actions' && typeof init?.body === 'string') {
      const action = JSON.parse(init.body)
      if (action.type === 'message.send') {
        const count = this.messageActions.push(action)
        const hash = new Uint8Array(await crypto.subtle.digest('SHA-256', new TextEncoder().encode(action.idempotency_key)))
        this.echoMessageId = `message/${[...hash.slice(0, 8)].map((byte) => byte.toString(16).padStart(2, '0')).join('')}`
        this.readyActions.add(count)
        for (let index = 0; index < this.actionArrivals.length;) {
          const waiter = this.actionArrivals[index]!
          if (waiter.count === count) {
            this.actionArrivals.splice(index, 1)
            waiter.arrived()
          } else index += 1
        }
        await this.sendGate
        if (this.transportFailure) throw new TypeError('Gateway disconnected')
        if (this.staleSendFence) return Response.json({
          api_version: 'st3.client.v0', error_version: 'st3.client.error.v0',
          code: 'stale-fence', message: 'The observed snapshot belongs to an old gateway store.',
          retryable: false, request_id: 'request/stale-send', details: {},
        }, { status: 409 })
        if (this.messageGrant !== 'granted') {
          return new Response(
            JSON.stringify({
              api_version: 'st3.client.v0',
              error_version: 'st3.client.error.v0',
              code: 'forbidden',
              message: 'control.messages is not granted',
              retryable: false,
              request_id: 'request/denied',
              details: {},
            }),
            { status: 403, headers: { 'content-type': 'application/json' } },
          )
        }
        const followed = this.commands.some(
          (command) =>
            command.kind === 'subscribe' &&
            command.collection === 'conversation' &&
            command.conversation === action.parameters.to,
        )
        if (!followed)
          throw new Error('Message addressed to a conversation that was never followed')
        value = {
          kind: 'action-result',
          action_id: action.id,
          operation_id: 'operation/send',
          status: this.rejectSend ? 'rejected' : 'completed',
          affected_ids: [this.echoMessageId],
          snapshot_id: snapshot.id,
        }
      } else if (action.type === 'terminal.attach') {
        value = {
          terminal_attachment: {
            runtime_incarnation: action.fence.runtime_incarnation,
            stream_capability: `capability-${action.parameters.target_id}`,
          },
        }
      } else {
        throw new Error(`Unexpected action ${action.type}`)
      }
    } else if (path === '/v1/client/usage') {
      value = { since_ms: 0, until_ms: 1000, rows: [] }
    } else {
      throw new Error(`Unexpected request ${path}`)
    }
    return new Response(JSON.stringify({ api_version: 'st3.client.v0', snapshot: this.currentSnapshot, value }), {
      status: 200,
      headers: { 'content-type': 'application/json' },
    })
  }

  readonly factory = () => {
    const socket: CollectionSocket = {
      onopen: null,
      onmessage: null,
      onclose: null,
      onerror: null,
      send: (text: string) => this.commands.push(JSON.parse(text)),
      close: () => {},
    }
    this.socket = socket
    queueMicrotask(() => socket.onopen?.())
    return socket
  }

  subscription(collection: string) {
    const command = this.commands.findLast(
      (entry) => entry.kind === 'subscribe' && entry.collection === collection,
    )
    if (command === undefined) throw new Error(`No subscription for ${collection}`)
    return command
  }

  send(frame: CollectionFrame) {
    this.socket?.onmessage?.({ data: JSON.stringify(frame) })
  }

  mailEcho() {
    this.send({
      kind: 'conversation', id: this.subscription('conversation').id,
      collection: 'conversation', session_id: 'session/example',
      replace: false, has_more: false,
      items: [
        { id: 'timeline-entry/echo/message', sequence: 1, revision: 1,
          type: 'message', role: 'user', final: true, timestamp: snapshot.created_at,
          body: { message_id: this.echoMessageId, from: 'person/operator', to: agent.id } },
        { id: 'timeline-entry/echo/content', sequence: 2, revision: 1,
          type: 'content', role: 'user', final: true, timestamp: snapshot.created_at,
          body: { media_type: 'text/plain', text: 'hello' } },
      ],
    })
  }

  fleet(rows: Agent[]) {
    this.send({
      kind: 'snapshot',
      id: this.subscription('agents').id,
      collection: 'agents',
      has_more: false,
      items: rows,
      order: rows.map((row) => row.id),
      snapshot,
    })
  }

  screen(id: string, name: string, text: string) {
    const value: TerminalScreen = {
      kind: 'terminal-screen',
      terminal_id: `terminal/${name}`,
      runtime_incarnation: `incarnation-${name}`,
      revision: '1',
      next_sequence: 1,
      columns: 80,
      rows: 24,
      title: name,
      truncated: false,
      cursor: { blinking: false, column: 0, row: 0, style: 'block', visible: true },
      lines: [{ row: 0, text, runs: [], redacted: false, truncated: false }],
      modes: {
        alternate_screen: false,
        application_cursor: false,
        application_keypad: false,
        bracketed_paste: false,
        focus_events: false,
        mouse_encoding: 'default',
        mouse_tracking: 'none',
      },
    }
    this.send({ kind: 'screen', id, collection: 'terminal', snapshot, value })
  }
}

let frames = new Map<number, FrameRequestCallback>()
beforeEach(() => {
  vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
  frames = new Map()
  let nextFrame = 0
  vi.stubGlobal('requestAnimationFrame', (callback: FrameRequestCallback) => {
    const id = (nextFrame += 1)
    frames.set(id, callback)
    return id
  })
  vi.stubGlobal('cancelAnimationFrame', (id: number) => frames.delete(id))
})
afterEach(() => {
  vi.unstubAllGlobals()
  vi.useRealTimers()
})

/** Drain socket callbacks, browser-task decode slices, stream fibers and frame-ingest commits. */
const settle = Effect.promise(async () => {
  for (let round = 0; round < 10; round += 1) {
    await new Promise<void>((resolve) => setImmediate(resolve))
    // Fake browsers place zero-delay tasks scheduled inside another timer on the next tick.
    await vi.advanceTimersByTimeAsync(1)
    const pending = [...frames.values()]
    frames.clear()
    for (const callback of pending) callback(0)
  }
})

const withGateway = (
  test: (live: LiveSource, gateway: Gateway) => Effect.Effect<void>,
  { maxFollows = 2, sessionTraceLayer }: {
    readonly maxFollows?: number
    readonly sessionTraceLayer?: Layer.Layer<SessionTraceProvider>
  } = {},
) =>
  Effect.gen(function* () {
    const gateway = new Gateway()
    const live = yield* Effect.acquireRelease(
      Effect.sync(() =>
        liveSource({
          sessionTraceLayer,
          options: {
            baseUrl: 'http://gateway.test',
            maxFollows,
            socket: gateway.factory,
            fetch: gateway.fetch,
            onFollowSubscribeSent: (event) => gateway.sentFollowSubscribes.push(event),
          },
        }),
      ),
      (live) => Effect.promise(() => live.dispose()),
    )
    yield* test(live, gateway)
  }).pipe(Effect.scoped)

it.live('exposes the native default and an injected trace provider through the owned source runtime', () =>
  withGateway((live) => Effect.gen(function* () {
    const query = { native_session_id: 'native-example', range: '7d', bucket: '15m' } as const
    expect(live.source.sessionTrace).toBeDefined()
    const native = yield* Effect.promise(() => live.source.sessionTrace!(query))
    expect(native.partial).toBe(true)
    expect(native.meters.scope).toBe('agent-incarnations')
    expect(native.buckets).toEqual({ _tag: 'Unknown', reason: 'no-provider' })
    const full = { ...native, scope: 'including_subagents' as const, partial: false,
      buckets: { _tag: 'Known' as const, value: [] }, subagents: { _tag: 'Known' as const, value: [] } }
    yield* withGateway((injected) => Effect.gen(function* () {
      expect(yield* Effect.promise(() => injected.source.sessionTrace!(query))).toEqual(full)
    }), {
      sessionTraceLayer: Layer.succeed(SessionTraceProvider, {
        load: () => Effect.succeed(full),
      }),
    })
  })),
)

describe('live feed sync sidecars', () => {
  it.live('records requested only after actual send and retains the last observed value as stale', () =>
    withGateway((live, gateway) =>
      Effect.gen(function* () {
        const sync = live.source.sync
        if (sync === undefined) throw new Error('Live sync sidecars are missing')
        const unmount = live.registry.mount(live.source.agents)
        yield* settle

        expect(gateway.sentFollowSubscribes).toEqual([
          { id: gateway.subscription('agents').id, key: 'window:agents:100:{}' },
        ])
        expect(live.registry.get(sync.agents).sync.status).toEqual({ _tag: 'Requested', since: expect.any(Number) })
        const agentsId = gateway.subscription('agents').id
        gateway.fleet([agent])
        yield* settle
        const observed = live.registry.get(sync.agents)
        expect(observed.sync.status._tag).toBe('Live')
        expect(Option.getOrUndefined(observed.last)).toMatchObject({
          value: [{ id: agent.id }],
        })
        const observedAt = Option.getOrUndefined(observed.last)?.observedAt
        const follow = gateway.subscription('agents')
        gateway.send({ kind: 'resync', id: agentsId, retryable: true })
        yield* settle

        const stale = live.registry.get(sync.agents)
        expect(stale.sync.status).toEqual({
          _tag: 'Stale',
          reason: { _tag: 'Unknown' },
          lastLiveAt: expect.any(Number),
        })
        expect(Option.getOrUndefined(stale.last)?.observedAt).toBe(observedAt)
        expect(live.registry.get(live.source.agents)).toMatchObject({ _tag: 'Observed', freshness: 'stale' })
        expect(gateway.subscription('agents').id).toBe(follow.id)
        unmount()
      }),
    ),
  )
  it.live('does not call an open gateway Live until a subscription data frame decodes', () =>
    withGateway((live, gateway) =>
      Effect.gen(function* () {
        const sync = live.source.sync!
        live.registry.mount(sync.gateway)
        live.registry.mount(live.source.agents)
        yield* settle
        expect(live.registry.get(sync.gateway).status).toEqual({ _tag: 'Requested', since: expect.any(Number) })
        gateway.send({ kind: 'resync', id: gateway.subscription('agents').id, retryable: true })
        yield* settle
        expect(live.registry.get(sync.gateway).status._tag).toBe('Requested')
        gateway.fleet([agent])
        yield* settle
        expect(live.registry.get(sync.gateway).status).toEqual({ _tag: 'Live', since: expect.any(Number) })
      }),
    ),
  )
  it.live('retains decoded rows and terminal Failed facts after a nonauthorization failure', () =>
    withGateway((live, gateway) =>
      Effect.gen(function* () {
        live.registry.mount(live.source.agents)
        yield* settle
        gateway.fleet([agent])
        yield* settle
        const sync = live.source.sync!
        const last = live.registry.get(sync.agents).last
        gateway.send({ kind: 'error', id: gateway.subscription('agents').id, collection: 'agents', code: 'unavailable', message: 'Owner host unavailable', retryable: false })
        yield* settle
        expect(live.registry.get(sync.agents).sync.status).toEqual({
          _tag: 'Failed', cause: { _tag: 'Server', code: 'unavailable', message: 'Owner host unavailable' },
        })
        expect(live.registry.get(sync.agents).last).toBe(last)
        expect(live.registry.get(live.source.agents)).toMatchObject({
          _tag: 'Observed', freshness: 'stale', value: [{ id: agent.id }],
          error: { reason: 'failed', detail: 'Owner host unavailable' },
        })
        yield* settle
        expect(live.registry.get(sync.agents).sync.status._tag).toBe('Failed')
      }),
    ),
  )
  it.live('reports a rejected socket credential as Failed on mounted and later-opened follows', () =>
    withGateway((live, gateway) =>
      Effect.gen(function* () {
        const sync = live.source.sync
        if (sync === undefined) throw new Error('Live sync sidecars are missing')
        gateway.rejectedCredential = true
        const failed = {
          _tag: 'Failed',
          cause: { _tag: 'Local', kind: 'connection-rejected', detail: { message: 'Device credential revoked' } },
        }
        live.registry.mount(live.source.agents)
        live.registry.mount(sync.agents)
        yield* settle
        expect(live.registry.get(live.source.agents)).toMatchObject({ _tag: 'Unavailable', reason: 'ungranted' })
        expect(live.registry.get(sync.agents).sync.status).toEqual(failed)
        expect(Option.isNone(live.registry.get(sync.agents).last)).toBe(true)
        const later = 'agent/opened-after-rejection'
        expect(live.registry.get(live.source.conversation(later))).toMatchObject({ _tag: 'Unavailable', reason: 'ungranted' })
        expect(live.registry.get(sync.conversation(later)).sync.status).toEqual(failed)
      }),
    ),
  )
})


describe('cold conversation admission', () => {
  it.live('does not subscribe any conversation without explicit visible selection', () =>
    withGateway((live, gateway) =>
      Effect.gen(function* () {
        live.registry.mount(live.source.agents)
        live.registry.mount(live.source.conversation('agent/first'))
        live.source.prefetchConversation?.('agent/first')
        yield* settle
        gateway.fleet([agent])
        yield* settle
        expect(gateway.commands.filter(
          (command) => command.kind === 'subscribe' && command.collection === 'conversation',
        )).toEqual([])
      }),
    ),
  )

  it.live('subscribes selected X first and blocks intent until its real first-page frame', () =>
    withGateway((live, gateway) =>
      Effect.gen(function* () {
        const selected = 'agent/X'
        live.source.prefetchConversation?.('agent/first')
        live.registry.mount(live.source.conversationInterest!(selected))
        live.source.prefetchConversation?.('agent/other')
        yield* settle
        const first = gateway.subscription('conversation')
        expect(gateway.sentFollowSubscribes.map((event) => event.key))
          .toEqual(['conversation:agent/X'])
        gateway.send({
          kind: 'conversation', id: first.id, collection: 'conversation',
          session_id: 'session/X', items: [], replace: true, has_more: false,
        })
        // Decode the real frame, without running the scheduled frame writer.
        for (let round = 0; round < 10; round += 1) {
          yield* Effect.promise(() => new Promise<void>((resolve) => setImmediate(resolve)))
          yield* Effect.promise(() => vi.advanceTimersByTimeAsync(1))
        }
        live.source.prefetchConversation?.('agent/other')
        expect(gateway.sentFollowSubscribes).toHaveLength(1)
        yield* settle
        // Early intent is not queued when the first page becomes visible.
        expect(gateway.sentFollowSubscribes).toHaveLength(1)
        live.source.prefetchConversation?.('agent/other')
        yield* settle
        expect(gateway.sentFollowSubscribes.map((event) => event.key))
          .toEqual(['conversation:agent/X', 'conversation:agent/other'])
      }),
    ),
  )
})


describe('terminal dependent-read authority', () => {
  for (const status of [401, 403, 503]) {
    it.live(`handles replacement runtime refusal ${status} without disclosing revoked content`, () =>
      withGateway((live, gateway) =>
        Effect.gen(function* () {
          live.registry.mount(live.source.agents)
          yield* settle
          gateway.fleet([agent])
          yield* settle
          const ref = 'terminal/example'
          live.registry.mount(live.source.terminalInterest!(ref))
          yield* settle
          gateway.screen(gateway.subscription('terminal').id, 'old', 'Trusted previous screen')
          yield* settle
          expect(live.registry.get(live.source.terminal(ref))._tag).toBe('Observed')
          gateway.runtimeRefusalStatus = status
          gateway.fleet([{ ...agent, revision: '2', runtime_ids: ['runtime/new'] }])
          yield* settle
          const feed = live.registry.get(live.source.terminal(ref))
          if (status === 503)
            expect(feed).toMatchObject({ _tag: 'Observed', freshness: 'stale', error: { reason: 'failed' } })
          else
            expect(feed).toMatchObject({ _tag: 'Unavailable', reason: 'ungranted' })
          expect(live.registry.get(live.source.sync!.terminal(ref)).sync.status).toEqual({
            _tag: 'Failed', cause: { _tag: 'Server', code: status === 503 ? 'unavailable' : 'forbidden', message: 'Runtime read refused' },
          })
        }),
      ),
    )
  }

  it.live('clears a terminal when its dependent roster read is revoked on an open socket', () =>
    withGateway((live, gateway) =>
      Effect.gen(function* () {
        live.registry.mount(live.source.agents)
        yield* settle
        gateway.fleet([agent])
        yield* settle
        const ref = 'terminal/example'
        live.registry.mount(live.source.terminalInterest!(ref))
        yield* settle
        gateway.screen(gateway.subscription('terminal').id, 'old', 'Previously authorized screen')
        yield* settle
        expect(live.registry.get(live.source.terminal(ref))._tag).toBe('Observed')
        gateway.send({ kind: 'error', id: gateway.subscription('agents').id, collection: 'agents', code: 'forbidden', message: 'Roster read revoked', retryable: false })
        yield* settle
        expect(live.registry.get(live.source.terminal(ref))).toMatchObject({ _tag: 'Unavailable', reason: 'ungranted' })
        expect(live.registry.get(live.source.sync!.terminal(ref)).sync.status).toEqual({ _tag: 'Failed', cause: { _tag: 'Unknown' } })
      }),
    ),
  )
})

describe('optimistic conversation sends', () => {
  const request: AttachmentSendRequest = {
    _tag: 'Send',
    api_version: 'st3.client.v0', type: 'message.send', id: 'action/optimistic',
    parameters: { to: agent.id, content: 'hello', tags: [], attachments: [] },
  }

  it.live('owns the send fence and uses the freshest held gateway snapshot', () =>
    withGateway((live, gateway) =>
      Effect.gen(function* () {
        live.registry.mount(live.source.agents)
        live.registry.mount(live.source.conversationInterest!(agent.id))
        yield* settle
        const current = { ...snapshot, id: 'snapshot/current', store_index: 7 }
        gateway.send({
          kind: 'snapshot', id: gateway.subscription('agents').id, collection: 'agents',
          has_more: false, items: [agent], order: [agent.id], snapshot: current,
        })
        yield* settle
        const reads = gateway.capabilityReads
        expect((yield* Effect.promise(() => live.source.attachments!.send(request)))._tag).toBe('Success')
        expect(gateway.messageActions[0]?.fence).toEqual({ snapshot_id: current.id, subject_revisions: {} })
        // Only the send permission read, not an unnecessary HTTP snapshot acquisition.
        expect(gateway.capabilityReads - reads).toBe(1)
      }),
    ),
  )

  it.live('publishes Pending in the same tick while an unheld snapshot is being read', () =>
    withGateway((live, gateway) =>
      Effect.gen(function* () {
        live.registry.mount(live.source.conversationInterest!(agent.id))
        const conversation = live.source.conversation(agent.id)
        live.registry.mount(conversation)
        yield* settle
        let release!: () => void
        gateway.capabilityGate = new Promise<void>((resolve) => { release = resolve })
        gateway.currentSnapshot = { ...snapshot, id: 'snapshot/http-current', store_index: 8 }
        const sending = live.source.attachments!.send(request)
        expect(live.registry.get(conversation)).toMatchObject({
          _tag: 'Observed', value: { items: [{ _tag: 'Text', sendState: { _tag: 'Pending' } }] },
        })
        yield* settle
        expect(gateway.messageActions).toHaveLength(0)
        release()
        expect((yield* Effect.promise(() => sending))._tag).toBe('Success')
        expect(gateway.messageActions[0]?.fence.snapshot_id).toBe('snapshot/http-current')
      }),
    ),
  )

  it.live('retains a typed stale-fence failure and resends the same key after refresh', () =>
    withGateway((live, gateway) =>
      Effect.gen(function* () {
        live.registry.mount(live.source.agents)
        live.registry.mount(live.source.conversationInterest!(agent.id))
        const conversation = live.source.conversation(agent.id)
        live.registry.mount(conversation)
        yield* settle
        gateway.fleet([agent])
        yield* settle
        gateway.staleSendFence = true
        const refused = yield* Effect.promise(() => live.source.attachments!.send(request))
        expect(refused).toMatchObject({
          _tag: 'Refused', reason: 'stale-fence', detail: 'The observed snapshot belongs to an old gateway store.',
          error: { code: 'stale-fence' },
        })
        expect(live.registry.get(conversation)).toMatchObject({
          _tag: 'Observed', value: { items: [{
            _tag: 'Text', text: 'hello',
            sendState: { _tag: 'Failed', reason: 'stale-fence', detail: refused._tag === 'Refused' ? refused.detail : '' },
          }] },
        })
        const first = gateway.messageActions[0]!
        gateway.staleSendFence = false
        const refreshed = { ...snapshot, id: 'snapshot/refreshed', store_index: 9 }
        gateway.send({
          kind: 'changes', id: gateway.subscription('agents').id, collection: 'agents',
          has_more: false, upserts: [], removes: [], order: [agent.id], snapshot: refreshed,
        })
        yield* settle
        expect((yield* Effect.promise(() => live.source.attachments!.send({
          ...request, _tag: 'Resend', idempotencyKey: first.idempotency_key,
        })))._tag).toBe('Success')
        expect(gateway.messageActions[1]).toMatchObject({
          id: first.id, idempotency_key: first.idempotency_key, parameters: first.parameters,
          fence: { snapshot_id: refreshed.id, subject_revisions: {} },
        })
        expect(live.registry.get(conversation)).toMatchObject({
          _tag: 'Observed', value: { items: [{ _tag: 'Text', sendState: { _tag: 'Sent' } }] },
        })
      }),
    ),
  )

  it.live('returns a typed failure and retains the row when no snapshot is obtainable', () =>
    withGateway((live, gateway) =>
      Effect.gen(function* () {
        live.registry.mount(live.source.conversationInterest!(agent.id))
        const conversation = live.source.conversation(agent.id)
        live.registry.mount(conversation)
        yield* settle
        gateway.currentSnapshot = undefined
        const result = yield* Effect.promise(() => live.source.attachments!.send(request))
        expect(result).toMatchObject({
          _tag: 'Refused', reason: 'snapshot-unavailable', detail: expect.stringContaining('snapshot'),
        })
        expect(gateway.messageActions).toHaveLength(0)
        expect(live.registry.get(conversation)).toMatchObject({
          _tag: 'Observed', value: { items: [{
            _tag: 'Text', text: 'hello',
            sendState: { _tag: 'Failed', reason: 'snapshot-unavailable', detail: expect.stringContaining('snapshot') },
          }] },
        })
      }),
    ),
  )

  it.live('keeps outbox rows in send-time order across replies, retries, equal-time rows and echoes', () => {
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout', 'Date'] })
    vi.setSystemTime('2026-10-08T12:00:01.000Z')
    return withGateway((live, gateway) =>
      Effect.gen(function* () {
        live.registry.mount(live.source.conversationInterest!(agent.id))
        const conversation = live.source.conversation(agent.id)
        live.registry.mount(conversation)
        yield* settle
        const content = (id: string, sequence: number, timestamp: string) => ({
          id, sequence, revision: 1, type: 'content' as const, role: 'assistant' as const,
          final: true, timestamp, body: { media_type: 'text/plain', text: id },
        })
        const page = (replace: boolean) => gateway.send({
          kind: 'conversation', id: gateway.subscription('conversation').id,
          collection: 'conversation', session_id: 'session/example', replace, has_more: false,
          items: [
            content('history', 1, '2026-10-08T12:00:00Z'),
            content('equal-time', 2, sendAt.replace('.000Z', 'Z')),
            content('reply', 3, '2026-10-08T12:00:04Z'),
          ],
        })
        const observed = () => {
          const feed = live.registry.get(conversation)
          if (feed._tag !== 'Observed') throw new Error('Expected an observed conversation')
          return feed.value
        }
        // Equal-time transcript content precedes an own send, even with differing ISO precision.
        const sendAt = '2026-10-08T12:00:01.000Z'
        gateway.send({
          kind: 'conversation', id: gateway.subscription('conversation').id,
          collection: 'conversation', session_id: 'session/example', replace: true, has_more: false,
          items: [content('history', 1, '2026-10-08T12:00:00Z'), content('equal-time', 2, sendAt)],
        })
        yield* settle
        vi.setSystemTime(sendAt)
        let release!: () => void
        gateway.sendGate = new Promise<void>((resolve) => { release = resolve })
        const sending = live.source.attachments!.send(request)
        const pendingRow = observed().items.at(-1)!
        expect(pendingRow).toMatchObject({ _tag: 'Text', at: sendAt, sendState: { _tag: 'Pending' } })
        yield* Effect.promise(() => gateway.nextAction())
        // The reply arrives while the send is Pending, not above its original position.
        page(false)
        yield* settle
        expect(observed().items.map(item => item.id)).toEqual(['history', 'equal-time', pendingRow.id, 'reply'])
        gateway.transportFailure = true
        release()
        expect((yield* Effect.promise(() => sending))._tag).toBe('Refused')
        expect(observed().items[2]).toMatchObject({ id: pendingRow.id, at: sendAt, sendState: { _tag: 'Failed' } })
        const failedPage = observed()
        expect(failedPage.change).toMatchObject({ from: expect.any(Array), index: 2 })
        // Replace/no-op frames must not shuffle the failed row past the reply.
        page(true)
        yield* settle
        expect(observed().items.map(item => item.id)).toEqual(['history', 'equal-time', pendingRow.id, 'reply'])
        const beforeRetry = observed()
        gateway.transportFailure = false
        gateway.sendGate = new Promise<void>((resolve) => { release = resolve })
        vi.setSystemTime('2026-10-08T12:00:05.000Z')
        const retrying = live.source.attachments!.send({
          ...request, _tag: 'Resend', idempotencyKey: gateway.messageActions[0]!.idempotency_key,
        })
        yield* Effect.promise(() => gateway.nextAction(2))
        yield* settle
        expect(observed().items[2]).toMatchObject({ id: pendingRow.id, at: sendAt, sendState: { _tag: 'Pending' } })
        expect(observed().change?.from).toBe(beforeRetry.items)
        expect(observed().change?.index).toBe(2)
        release()
        expect((yield* Effect.promise(() => retrying))._tag).toBe('Success')
        expect(observed().items[2]).toMatchObject({ id: pendingRow.id, at: sendAt, sendState: { _tag: 'Sent' } })
        expect(gateway.messageActions.map(action => action.idempotency_key)).toEqual([
          gateway.messageActions[0]!.idempotency_key, gateway.messageActions[0]!.idempotency_key,
        ])
        // A second send belongs after the reply; retry did not move the first send after it.
        yield* Effect.promise(() => live.source.attachments!.send(request))
        const secondRow = observed().items.at(-1)!
        expect(observed().items.map(item => item.id)).toEqual(['history', 'equal-time', pendingRow.id, 'reply', secondRow.id])
        gateway.mailEcho()
        yield* settle
        expect(observed().items.some(item => item.id === secondRow.id)).toBe(false)
        expect(observed().items.some(item => item.id === pendingRow.id)).toBe(true)
        expect(observed().items.filter(item => item._tag === 'Message')).toHaveLength(1)
      }),
    )
  })

  for (const echoFirst of [false, true]) {
    it.live(`publishes pending prose synchronously and replaces it when echo arrives ${echoFirst ? 'before' : 'after'} POST resolves`, () =>
      withGateway((live, gateway) =>
        Effect.gen(function* () {
          live.registry.mount(live.source.conversationInterest!(agent.id))
          const conversation = live.source.conversation(agent.id)
          live.registry.mount(conversation)
          yield* settle
          let resolvePost!: () => void
          gateway.sendGate = new Promise<void>((resolve) => { resolvePost = resolve })
          const sending = live.source.attachments!.send(request)
          // No microtask, timer or animation frame occurs between send and this read.
          expect(live.registry.get(conversation)).toMatchObject({
            _tag: 'Observed', value: { items: [
              { _tag: 'Text', role: 'user', text: 'hello', sendState: { _tag: 'Pending' } },
            ] },
          })
          gateway.send({
            kind: 'conversation', id: gateway.subscription('conversation').id,
            collection: 'conversation', session_id: 'session/example',
            replace: true, has_more: false, items: [],
          })
          yield* settle
          expect(live.registry.get(conversation)).toMatchObject({
            _tag: 'Observed', value: { items: [
              { _tag: 'Text', text: 'hello', sendState: { _tag: 'Pending' } },
            ] },
          })
          // The identity is only knowable once the gateway sees the client-chosen key.
          if (echoFirst) {
            yield* Effect.promise(() => gateway.nextAction())
            expect(gateway.echoMessageId).toMatch(/^message\/[0-9a-f]{16}$/)
            yield* settle
            gateway.mailEcho()
            yield* settle
            expect(live.registry.get(conversation)).toMatchObject({
              _tag: 'Observed', value: { items: [
                { _tag: 'Message', messageId: gateway.echoMessageId },
                { _tag: 'Text', id: 'timeline-entry/echo/content', text: 'hello' },
              ] },
            })
          }
          resolvePost()
          expect((yield* Effect.promise(() => sending))._tag).toBe('Success')
          if (!echoFirst) gateway.mailEcho()
          yield* settle
          const feed = live.registry.get(conversation)
          expect(feed._tag).toBe('Observed')
          if (feed._tag !== 'Observed') return
          expect(feed.value.items.filter((item) => item._tag === 'Text')).toEqual([
            expect.objectContaining({ id: 'timeline-entry/echo/content', text: 'hello' }),
          ])
          expect(feed.value.items.every((item) => item._tag !== 'Text' || item.sendState === undefined)).toBe(true)
          expect(gateway.messageActions[0]?.idempotency_key).toMatch(/^[0-9a-f-]{36}$/)
        }),
      ),
    )
  }

  for (const failure of ['rejected', 'failed', 'ungranted'] as const) {
    it.live(`retains typed failed prose on ${failure} send`, () =>
      withGateway((live, gateway) =>
        Effect.gen(function* () {
          live.registry.mount(live.source.conversationInterest!(agent.id))
          const conversation = live.source.conversation(agent.id)
          live.registry.mount(conversation)
          yield* settle
          gateway.rejectSend = failure === 'rejected'
          gateway.transportFailure = failure === 'failed'
          gateway.messageGrant = failure === 'ungranted' ? 'ungranted' : 'granted'
          yield* Effect.promise(() => live.source.attachments!.send(request))
          expect(live.registry.get(conversation)).toMatchObject({
            _tag: 'Observed', value: { items: [
              { _tag: 'Text', role: 'user', text: 'hello', sendState: { _tag: 'Failed', reason: failure } },
            ] },
          })
        }),
      ),
    )
  }

  it.live('normalizes wire-null attachment names in the optimistic row', () =>
    withGateway((live, gateway) =>
      Effect.gen(function* () {
        live.registry.mount(live.source.conversationInterest!(agent.id))
        const conversation = live.source.conversation(agent.id)
        live.registry.mount(conversation)
        yield* settle
        const sending = live.source.attachments!.send({
          ...request,
          parameters: {
            ...request.parameters,
            content: '',
            attachments: [{ blob: `blob/${'b'.repeat(64)}`, media_type: 'image/webp', name: null }],
          },
        })
        const observed = live.registry.get(conversation)
        expect(observed).toMatchObject({ _tag: 'Observed' })
        // Exact equality: a wire-null name must be absent, never published as null.
        if (observed._tag === 'Observed')
          expect(observed.value.items[0]).toEqual({
            _tag: 'Text', id: expect.stringMatching(/^pending\//), role: 'user', text: '',
            attachments: [{ id: `blob/${'b'.repeat(64)}`, mediaType: 'image/webp' }],
            streaming: false, at: expect.any(String), sendState: { _tag: 'Pending' },
          })
        expect(yield* Effect.promise(() => sending)).toMatchObject({ _tag: 'Success' })
      }),
    ),
  )

  it.live('never republishes transcript rows after the conversation read is refused', () =>
    withGateway((live, gateway) =>
      Effect.gen(function* () {
        live.registry.mount(live.source.conversationInterest!(agent.id))
        const conversation = live.source.conversation(agent.id)
        live.registry.mount(conversation)
        yield* settle
        gateway.send({
          kind: 'conversation', id: gateway.subscription('conversation').id,
          collection: 'conversation', session_id: 'session/example',
          replace: true, has_more: false,
          items: [
            { id: 'timeline-entry/secret/message', sequence: 1, revision: 1,
              type: 'message', role: 'user', final: true, timestamp: snapshot.created_at,
              body: { message_id: 'message/secret', from: 'person/operator', to: agent.id } },
            { id: 'timeline-entry/secret/content', sequence: 2, revision: 1,
              type: 'content', role: 'user', final: true, timestamp: snapshot.created_at,
              body: { media_type: 'text/plain', text: 'authorized rows' } },
          ],
        })
        yield* settle
        expect(live.registry.get(conversation)).toMatchObject({ _tag: 'Observed' })
        gateway.send({
          kind: 'error', id: gateway.subscription('conversation').id, collection: 'conversation',
          code: 'forbidden', message: 'Conversation read revoked', retryable: false,
        })
        yield* settle
        expect(live.registry.get(conversation)).toMatchObject({ _tag: 'Unavailable', reason: 'ungranted' })
        let resolvePost!: () => void
        gateway.sendGate = new Promise<void>((resolve) => { resolvePost = resolve })
        const sending = live.source.attachments!.send(request)
        expect(live.registry.get(conversation)).toMatchObject({ _tag: 'Unavailable', reason: 'ungranted' })
        resolvePost()
        expect((yield* Effect.promise(() => sending))._tag).toBe('Success')
        yield* settle
        // A local outbox update must never resurrect rows the gateway stopped authorizing.
        expect(live.registry.get(conversation)).toMatchObject({ _tag: 'Unavailable', reason: 'ungranted' })
      }),
    ),
  )

  it.live('never duplicates mail an idempotent resubmission already shows', () =>
    withGateway((live, gateway) =>
      Effect.gen(function* () {
        const key = 'resend-stable-key'
        const hash = new Uint8Array(yield* Effect.promise(() => crypto.subtle.digest('SHA-256', new TextEncoder().encode(key))))
        const resent = `message/${[...hash.slice(0, 8)].map((byte) => byte.toString(16).padStart(2, '0')).join('')}`
        live.registry.mount(live.source.conversationInterest!(agent.id))
        const conversation = live.source.conversation(agent.id)
        live.registry.mount(conversation)
        yield* settle
        gateway.send({
          kind: 'conversation', id: gateway.subscription('conversation').id,
          collection: 'conversation', session_id: 'session/example',
          replace: true, has_more: false,
          items: [
            { id: 'timeline-entry/resent/message', sequence: 1, revision: 1,
              type: 'message', role: 'user', final: true, timestamp: snapshot.created_at,
              body: { message_id: resent, from: 'person/operator', to: agent.id } },
            { id: 'timeline-entry/resent/content', sequence: 2, revision: 1,
              type: 'content', role: 'user', final: true, timestamp: snapshot.created_at,
              body: { media_type: 'text/plain', text: 'hello' } },
          ],
        })
        yield* settle
        let resolvePost!: () => void
        gateway.sendGate = new Promise<void>((resolve) => { resolvePost = resolve })
        const sending = live.source.attachments!.send({ ...request, _tag: 'Resend', idempotencyKey: key })
        yield* Effect.promise(() => gateway.nextAction())
        yield* settle
        // The authoritative copy is already in the window: no second, pending copy.
        const feed = live.registry.get(conversation)
        expect(feed).toMatchObject({ _tag: 'Observed' })
        if (feed._tag !== 'Observed') return
        expect(feed.value.items.map((item) => item._tag)).toEqual(['Message', 'Text'])
        resolvePost()
        expect((yield* Effect.promise(() => sending))._tag).toBe('Success')
        yield* settle
        expect(live.registry.get(conversation)).toMatchObject({
          _tag: 'Observed', value: { items: [
            { _tag: 'Message', messageId: resent },
            { _tag: 'Text', id: 'timeline-entry/resent/content' },
          ] },
        })
      }),
    ),
  )

  it.live('publishes an unseen idempotent resubmission as pending before the POST resolves', () =>
    withGateway((live, gateway) =>
      Effect.gen(function* () {
        live.registry.mount(live.source.conversationInterest!(agent.id))
        live.registry.mount(live.source.conversation(agent.id))
        yield* settle
        let resolvePost!: () => void
        gateway.sendGate = new Promise<void>((resolve) => { resolvePost = resolve })
        const sending = live.source.attachments!.send({ ...request, _tag: 'Resend', idempotencyKey: 'unseen-resend-key' })
        yield* Effect.promise(() => gateway.nextAction())
        yield* settle
        expect(live.registry.get(live.source.conversation(agent.id))).toMatchObject({
          _tag: 'Observed', value: { items: [
            { _tag: 'Text', text: 'hello', sendState: { _tag: 'Pending' } },
          ] },
        })
        resolvePost()
        expect((yield* Effect.promise(() => sending))._tag).toBe('Success')
      }),
    ),
  )

  it.live('settles a completed send whose mailbox echo never enters the window', () =>
    withGateway((live, gateway) =>
      Effect.gen(function* () {
        live.registry.mount(live.source.conversationInterest!(agent.id))
        const conversation = live.source.conversation(agent.id)
        live.registry.mount(conversation)
        yield* settle
        gateway.sendGate = Promise.resolve()
        yield* Effect.promise(() => live.source.attachments!.send(request))
        yield* settle
        // Completed authoritatively, but no mailbox frame ever repeats it: the row stays,
        // visibly settled rather than pending forever.
        expect(live.registry.get(conversation)).toMatchObject({
          _tag: 'Observed', value: { items: [
            { _tag: 'Text', role: 'user', text: 'hello', sendState: { _tag: 'Sent' } },
          ] },
        })
        // A later in-window echo still replaces the settled row exactly once.
        yield* Effect.promise(() => gateway.nextAction())
        gateway.mailEcho()
        yield* settle
        const feed = live.registry.get(conversation)
        expect(feed).toMatchObject({ _tag: 'Observed' })
        if (feed._tag !== 'Observed') return
        expect(feed.value.items.filter((item) => item._tag === 'Text')).toEqual([
          expect.objectContaining({ id: 'timeline-entry/echo/content', text: 'hello' }),
        ])
      }),
    ),
  )

  it.live('keeps a confirmed send Sent when a later subscription page lacks its echo, until the echo arrives', () =>
    withGateway((live, gateway) =>
      Effect.gen(function* () {
        live.registry.mount(live.source.conversationInterest!(agent.id))
        const conversation = live.source.conversation(agent.id)
        live.registry.mount(conversation)
        yield* settle
        gateway.sendGate = Promise.resolve()
        yield* Effect.promise(() => live.source.attachments!.send(request))
        yield* settle
        expect(live.registry.get(conversation)).toMatchObject({
          _tag: 'Observed', value: { items: [
            { _tag: 'Text', text: 'hello', sendState: { _tag: 'Sent' } },
          ] },
        })
        // The socket drops; the SDK reopens and subscribes afresh after the send completed.
        gateway.socket?.onclose?.({ code: 1006, reason: 'socket dropped' })
        yield* Effect.promise(() => vi.advanceTimersByTimeAsync(500))
        yield* settle
        gateway.send({
          kind: 'conversation', id: gateway.subscription('conversation').id,
          collection: 'conversation', session_id: 'session/example',
          replace: true, has_more: false, items: [],
        })
        yield* settle
        // A later page may still precede owner-side replication of the confirmed send.
        expect(live.registry.get(conversation)).toMatchObject({
          _tag: 'Observed', value: { items: [
            { _tag: 'Text', text: 'hello', sendState: { _tag: 'Sent' } },
          ] },
        })
        yield* Effect.promise(() => gateway.nextAction())
        gateway.mailEcho()
        yield* settle
        const feed = live.registry.get(conversation)
        expect(feed).toMatchObject({ _tag: 'Observed' })
        if (feed._tag !== 'Observed') return
        expect(feed.value.items.filter((item) => item._tag === 'Text')).toEqual([
          expect.objectContaining({ id: 'timeline-entry/echo/content', text: 'hello' }),
        ])
      }),
    ),
  )

  it.live('keeps a confirmed send Sent when its current subscription page lacks its echo', () =>
    withGateway((live, gateway) =>
      Effect.gen(function* () {
        live.registry.mount(live.source.conversationInterest!(agent.id))
        const conversation = live.source.conversation(agent.id)
        live.registry.mount(conversation)
        yield* settle
        gateway.sendGate = Promise.resolve()
        yield* Effect.promise(() => live.source.attachments!.send(request))
        yield* settle
        // An empty page is not proof that the completed send is visible to the session owner.
        gateway.send({
          kind: 'conversation', id: gateway.subscription('conversation').id,
          collection: 'conversation', session_id: 'session/example',
          replace: true, has_more: false, items: [],
        })
        yield* settle
        expect(live.registry.get(conversation)).toMatchObject({
          _tag: 'Observed', value: { items: [
            { _tag: 'Text', text: 'hello', sendState: { _tag: 'Sent' } },
          ] },
        })
      }),
    ),
  )

  it.live('keeps a send that settles after follow eviction Sent on a later window without its echo', () =>
    withGateway((live, gateway) =>
      Effect.gen(function* () {
        const unmountInterest = live.registry.mount(live.source.conversationInterest!(agent.id))
        const conversation = live.source.conversation(agent.id)
        const unmountConversation = live.registry.mount(conversation)
        yield* settle
        const post = Promise.withResolvers<void>()
        gateway.sendGate = post.promise
        const sending = live.source.attachments!.send(request)
        yield* Effect.promise(() => gateway.nextAction())
        // The reader leaves while the POST is in flight; two more conversations exceed the
        // two-slot budget, so the evicted follow run ends before the send settles.
        unmountConversation()
        unmountInterest()
        const unmountOtherInterest = live.registry.mount(live.source.conversationInterest!('agent/other'))
        const unmountOther = live.registry.mount(live.source.conversation('agent/other'))
        const unmountThirdInterest = live.registry.mount(live.source.conversationInterest!('agent/third'))
        const unmountThird = live.registry.mount(live.source.conversation('agent/third'))
        yield* settle
        post.resolve()
        expect((yield* Effect.promise(() => sending))._tag).toBe('Success')
        unmountOther()
        unmountOtherInterest()
        unmountThird()
        unmountThirdInterest()
        // Returning opens a new follow, but its page still cannot prove send visibility.
        live.registry.mount(live.source.conversationInterest!(agent.id))
        live.registry.mount(conversation)
        yield* settle
        gateway.send({
          kind: 'conversation', id: gateway.subscription('conversation').id,
          collection: 'conversation', session_id: 'session/example',
          replace: true, has_more: false,
          items: [
            { id: 'timeline-entry/resumed/content', sequence: 1, revision: 1,
              type: 'content', role: 'assistant', final: true, timestamp: snapshot.created_at,
              body: { media_type: 'text/plain', text: 'resumed' } },
          ],
        })
        yield* settle
        expect(live.registry.get(conversation)).toMatchObject({
          _tag: 'Observed', value: {
            items: [
              { _tag: 'Text', id: 'timeline-entry/resumed/content', text: 'resumed' },
              { _tag: 'Text', text: 'hello', sendState: { _tag: 'Sent' } },
            ],
            change: { index: 0 },
          },
        })
      }),
    ),
  )
})
