import { it } from '@effect/vitest'
import type {
  Agent,
  CollectionCommand,
  CollectionFrame,
  CollectionSocket,
  Runtime,
  Snapshot,
  TerminalScreen,
  TimelineEntry,
} from '@smalltalk/st3-client'
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
        const followed = this.commands.findLast(
          (command) => command.kind === 'subscribe' && command.collection === 'conversation',
        )
        if (
          followed?.kind !== 'subscribe' ||
          followed.collection !== 'conversation' ||
          action.parameters.to !== followed.conversation
        ) {
          throw new Error('Message addressed to a different agent than the followed conversation')
        }
        value = {
          kind: 'action-result',
          action_id: action.id,
          operation_id: 'operation/send',
          status: 'completed',
          affected_ids: ['message/gateway-selected'],
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

/** A selected visible page must commit before explicit background intent is eligible. */
const selectAndPaint = (live: LiveSource, gateway: Gateway, ref = 'agent/selected') =>
  Effect.gen(function* () {
    live.registry.mount(live.source.conversationInterest!(ref))
    yield* settle
    gateway.send({
      kind: 'conversation',
      id: gateway.subscription('conversation').id,
      collection: 'conversation',
      session_id: `session/${ref.slice('agent/'.length)}`,
      items: [],
      replace: true,
      has_more: false,
    })
    yield* settle
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

