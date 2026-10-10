import type { CollectionSocket } from '@smalltalk/st3-client'
import { St3, St3Live } from '@st3/sdk/effect'
import { Effect, Stream } from 'effect'
import { expect, it } from 'vitest'
import { installPageLifecycle } from './pageLifecycle.ts'

const settle = Effect.promise(async () => {
  for (let round = 0; round < 10; round++) await new Promise<void>((resolve) => setImmediate(resolve))
})

it('pagehide sends 1001 synchronously and persisted pageshow resubscribes without a lifecycle timer', async () => {
  const sockets: { readonly socket: CollectionSocket; readonly commands: string[]; readonly closes: number[] }[] = []
  await Effect.gen(function* () {
    const st3 = yield* St3
    const target = new EventTarget()
    let resume: Effect.Effect<void> = Effect.void
    let disposed = false
    const uninstall = installPageLifecycle({ target, source: {
      suspendSockets: st3.suspendSockets,
      resumeSockets: () => { resume = st3.resumeSockets },
      dispose: async () => { disposed = true },
    } })
    yield* Effect.forkChild(st3.followWindow({ _tag: 'Window', collection: 'agents', limit: 100 }).pipe(Stream.runDrain))
    yield* settle
    expect(sockets).toHaveLength(1)
    expect(sockets[0]?.commands.filter((command) => JSON.parse(command).kind === 'subscribe')).toHaveLength(1)
    target.dispatchEvent(Object.assign(new Event('pagehide'), { persisted: true }))
    // No await, fiber drain, clock advancement, or scope disposal before the close assertion.
    expect(sockets[0]?.closes[0]).toBe(1001)
    expect(disposed).toBe(false)
    yield* settle
    expect(sockets).toHaveLength(1)
    target.dispatchEvent(Object.assign(new Event('pageshow'), { persisted: true }))
    yield* resume
    yield* settle
    expect(sockets).toHaveLength(2)
    expect(sockets[1]?.commands.filter((command) => JSON.parse(command).kind === 'subscribe')).toHaveLength(1)
    target.dispatchEvent(Object.assign(new Event('pagehide'), { persisted: false }))
    expect(sockets[1]?.closes[0]).toBe(1001)
    expect(disposed).toBe(true)
    uninstall()
  }).pipe(Effect.provide(St3Live({
    baseUrl: 'http://gateway.test', maxFollows: 4,
    fetch: async () => new Response(JSON.stringify({ api_version: 'st3.client.v0', snapshot: { id: 'snapshot/proof', created_at: '2026-10-08T00:00:00Z', host_id: 'host/proof', projection_version: 'client-projection.v0', store_index: 1 }, value: {} })),
    socket: () => {
      const commands: string[] = [], closes: number[] = []
      const socket: CollectionSocket = { onopen: null, onmessage: null, onclose: null, onerror: null, send: (command) => { commands.push(command) }, close: (code = 1000) => { closes.push(code) } }
      sockets.push({ socket, commands, closes })
      queueMicrotask(() => socket.onopen?.())
      return socket
    },
  })), Effect.scoped, Effect.runPromise)
})
