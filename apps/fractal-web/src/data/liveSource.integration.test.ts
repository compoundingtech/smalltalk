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
import { Effect } from 'effect'
import * as Option from 'effect/Option'
import * as Atom from 'effect/reactivity/Atom'
import { afterEach, beforeEach, describe, expect, vi } from 'vitest'

/** Cold-selection admission through the real live source and SDK. */

import { liveSource, type LiveSource } from './liveSource.ts'

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
  readonly messageActions: MessageSendAction[] = []
  /** Resolves once the gateway has derived the send action's message identity. */
  readonly nextAction = () =>
    this.echoMessageId !== ''
      ? Promise.resolve()
      : new Promise<void>((resolve) => this.actionArrivals.push(resolve))
  private readonly actionArrivals: Array<() => void> = []
  echoMessageId = ''

  readonly fetch: typeof fetch = async (input, init) => {
    const path = new URL(String(input)).pathname
    let value: unknown
    if (path === '/v1/client/capabilities') {
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
        this.messageActions.push(action)
        const hash = new Uint8Array(await crypto.subtle.digest('SHA-256', new TextEncoder().encode(action.idempotency_key)))
        this.echoMessageId = `message/${[...hash.slice(0, 8)].map((byte) => byte.toString(16).padStart(2, '0')).join('')}`
        this.actionArrivals.splice(0).forEach((arrived) => arrived())
        await this.sendGate
        if (this.transportFailure) throw new TypeError('Gateway disconnected')
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
    } else {
      throw new Error(`Unexpected request ${path}`)
    }
    return new Response(JSON.stringify({ api_version: 'st3.client.v0', snapshot, value }), {
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
  { maxFollows = 2 }: { readonly maxFollows?: number } = {},
) =>
  Effect.gen(function* () {
    const gateway = new Gateway()
    const live = yield* Effect.acquireRelease(
      Effect.sync(() =>
        liveSource({
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
    fence: { snapshot_id: snapshot.id, subject_revisions: {} },
    parameters: { to: agent.id, content: 'hello', tags: [], attachments: [] },
  }

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

  it.live('retires a settled send when a fresh subscription replaces the window without it', () =>
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
        // The first page of a subscription opened after completion is authoritative for
        // the window: an identity it does not contain retires the settled outbox row.
        expect(live.registry.get(conversation)).toMatchObject({
          _tag: 'Observed', value: { items: [] },
        })
      }),
    ),
  )

  it.live('keeps a settled send whose replace page predates its completion', () =>
    withGateway((live, gateway) =>
      Effect.gen(function* () {
        live.registry.mount(live.source.conversationInterest!(agent.id))
        const conversation = live.source.conversation(agent.id)
        live.registry.mount(conversation)
        yield* settle
        gateway.sendGate = Promise.resolve()
        yield* Effect.promise(() => live.source.attachments!.send(request))
        yield* settle
        // Same subscription the row settled under: the page may lag the send, so the row stays.
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

  it.live('retires a send that settles after its follow was evicted on the next fresh window', () =>
    withGateway((live, gateway) =>
      Effect.gen(function* () {
        const unmountInterest = live.registry.mount(live.source.conversationInterest!(agent.id))
        const conversation = live.source.conversation(agent.id)
        const unmountConversation = live.registry.mount(conversation)
        yield* settle
        let resolvePost!: () => void
        gateway.sendGate = new Promise<void>((resolve) => { resolvePost = resolve })
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
        resolvePost()
        expect((yield* Effect.promise(() => sending))._tag).toBe('Success')
        unmountOther()
        unmountOtherInterest()
        unmountThird()
        unmountThirdInterest()
        // Returning mounts a subscription opened after completion; its first window is
        // authoritative, retires the outbox row, and reports the whole page as changed.
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
            items: [{ _tag: 'Text', id: 'timeline-entry/resumed/content', text: 'resumed' }],
            change: { index: 0 },
          },
        })
      }),
    ),
  )
})
