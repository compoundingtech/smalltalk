import { createServer } from 'node:http'

import { it } from '@effect/vitest'
import type { ActionOf, CollectionSocket, ErrorEnvelope, Snapshot } from '@smalltalk/st3-client'
import { Effect } from 'effect'
import { describe, expect } from 'vitest'

import { ActionRefused, ActionTransportFailure, St3, St3Live } from './mod.ts'

const snapshot = (index: number): Snapshot => ({
  id: `snapshot/${index}`,
  created_at: '2026-10-03T00:00:00Z',
  host_id: 'host/build-a',
  projection_version: 'client-projection.v0',
  store_index: index,
})
const request: ActionOf<'message.send'> = {
  api_version: 'st3.client.v0',
  type: 'message.send',
  id: 'action/send',
  idempotency_key: 'composer-key',
  fence: { snapshot_id: 'snapshot/1', subject_revisions: {} },
  parameters: { to: 'agent/followed', content: 'Hello followed agent', tags: ['mission/example'] },
}
const refusal: ErrorEnvelope = {
  api_version: 'st3.client.v0',
  error_version: 'st3.client.error.v0',
  code: 'forbidden',
  message: 'control.messages is not granted',
  retryable: false,
  request_id: 'request/refusal',
  details: { scope: 'control.messages' },
}
const socket = (): CollectionSocket => {
  const value: CollectionSocket = {
    onopen: null,
    onmessage: null,
    onclose: null,
    onerror: null,
    send: () => {},
    close: () => {},
  }
  queueMicrotask(() => value.onopen?.())
  return value
}

const withGateway = <TError>(test: (baseUrl: string) => Effect.Effect<void, TError>) =>
  Effect.gen(function* () {
    let reads = 0
    const server = createServer(async (incoming, response) => {
      response.setHeader('content-type', 'application/json')
      if (incoming.method === 'GET' && incoming.url === '/v1/client/capabilities') {
        response.end(
          JSON.stringify({
            api_version: 'st3.client.v0',
            snapshot: snapshot(++reads),
            value: { limits: { max_page_items: 100, max_event_items: 100, max_wait_ms: 1000 } },
          }),
        )
        return
      }
      if (incoming.method !== 'POST' || incoming.url !== '/v1/client/actions') {
        response.writeHead(404).end()
        return
      }
      let body = ''
      for await (const chunk of incoming) body += chunk
      const action: ActionOf<'message.send'> = JSON.parse(body)
      // Refuse an unauthorized request at the HTTP boundary, not in a fake SDK service.
      if (action.parameters.to === 'agent/denied') {
        response.writeHead(403).end(JSON.stringify(refusal))
        return
      }
      if (action.parameters.to === 'agent/disconnected') {
        incoming.socket.destroy()
        return
      }
      if (
        action.type !== 'message.send' ||
        action.parameters.to !== 'agent/followed' ||
        action.parameters.content !== request.parameters.content ||
        action.fence.snapshot_id !== `snapshot/${reads}` ||
        incoming.headers.authorization !== undefined
      ) {
        response.writeHead(400).end(JSON.stringify({ ...refusal, code: 'validation-failed' }))
        return
      }
      response.end(
        JSON.stringify({
          api_version: 'st3.client.v0',
          snapshot: snapshot(reads),
          value: {
            kind: 'action-result',
            action_id: action.id,
            operation_id: 'operation/deliver',
            status: action.parameters.title === 'complete' ? 'completed' : 'accepted',
            affected_ids: ['message/delivered'],
            snapshot_id: `snapshot/${reads}`,
          },
        }),
      )
    })
    yield* Effect.acquireRelease(
      Effect.promise(() => new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve))),
      () =>
        Effect.promise(
          () =>
            new Promise<void>((resolve, reject) => {
              server.close((error) => (error === undefined ? resolve() : reject(error)))
              server.closeAllConnections()
            }),
        ),
    )
    const address = server.address()
    if (address === null || typeof address === 'string') return yield* Effect.die('No HTTP address')
    yield* test(`http://127.0.0.1:${address.port}`)
  }).pipe(Effect.scoped)

const withSdk = <TError>(
  baseUrl: string,
  test: (sdk: St3['Service']) => Effect.Effect<void, TError>,
) =>
  Effect.gen(function* () {
    yield* test(yield* St3)
  }).pipe(Effect.provide(St3Live({ baseUrl, maxFollows: 2, socket })))

describe('message.send HTTP transport', () => {
  it.live(
    'refreshes a cached discovery fence and preserves accepted/completed acknowledgements',
    () =>
      withGateway((baseUrl) =>
        withSdk(baseUrl, (sdk) =>
          Effect.gen(function* () {
            const first = yield* sdk.snapshot
            const fresh = yield* sdk.snapshot
            expect(fresh).not.toBe(first)
            const action = { ...request, fence: { ...request.fence, snapshot_id: fresh } }
            const accepted = yield* sdk.messageSend(action)
            expect(accepted).toMatchObject({
              kind: 'action-result',
              status: 'accepted',
              operation_id: 'operation/deliver',
              affected_ids: ['message/delivered'],
              snapshot_id: fresh,
            })
            const completed = yield* sdk.messageSend({
              ...action,
              parameters: { ...action.parameters, title: 'complete' },
            })
            expect(completed.status).toBe('completed')
          }),
        ),
      ),
  )

  it.live('returns the gateway canonical message identity independently of the idempotency key', () =>
    withGateway((baseUrl) =>
      withSdk(baseUrl, (sdk) =>
        Effect.gen(function* () {
          yield* sdk.snapshot
          // The gateway, not a composer-key hash, chooses the canonical message identity.
          for (const idempotencyKey of ['composer-key/first', 'composer-key/second']) {
            const fresh = yield* sdk.snapshot
            const result = yield* sdk.messageSend({
              ...request,
              idempotency_key: idempotencyKey,
              fence: { ...request.fence, snapshot_id: fresh },
            })
            expect(result.kind).toBe('action-result')
            expect(result.affected_ids).toEqual(['message/delivered'])
          }
        }),
      ),
    ),
  )

  it.live('keeps denied-scope code, retryability, HTTP status and refusal details typed', () =>
    withGateway((baseUrl) =>
      withSdk(baseUrl, (sdk) =>
        Effect.gen(function* () {
          const outcome = yield* sdk
            .messageSend({ ...request, parameters: { ...request.parameters, to: 'agent/denied' } })
            .pipe(
              Effect.match({
                onFailure: (error) => error,
                onSuccess: () => undefined,
              }),
            )
          expect(outcome).toBeInstanceOf(ActionRefused)
          expect(outcome).toMatchObject({ _tag: 'ActionRefused', status: 403, response: refusal })
        }),
      ),
    ),
  )

  it.live('distinguishes a lost HTTP response from a daemon refusal', () =>
    withGateway((baseUrl) =>
      withSdk(baseUrl, (sdk) =>
        Effect.gen(function* () {
          const outcome = yield* sdk
            .messageSend({
              ...request,
              parameters: { ...request.parameters, to: 'agent/disconnected' },
            })
            .pipe(
              Effect.match({
                onFailure: (error) => error,
                onSuccess: () => undefined,
              }),
            )
          expect(outcome).toBeInstanceOf(ActionTransportFailure)
          expect(outcome).toMatchObject({ _tag: 'ActionTransportFailure' })
        }),
      ),
    ),
  )
})
