import { createServer } from 'node:http'

import { Effect, Fiber, Stream } from 'effect'
import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import { expect, it } from 'vitest'

import recording from './subjectReadPort.gateway.fixtures.json' with { type: 'json' }
import { gatewaySubjectReads } from './subjectReadPort.ts'

it('aborts the last native reader while its HTTP body is incomplete and stops the scan', async () => {
  const started = Promise.withResolvers<void>()
  const closed = Promise.withResolvers<void>()
  let requests = 0
  const server = createServer((request, response) => {
    response.setHeader('Connection', 'close')
    if (request.url?.endsWith('/capabilities')) {
      response.end(JSON.stringify(recording.capabilities))
      return
    }
    requests++
    response.once('close', closed.resolve)
    response.writeHead(200, { 'content-type': 'application/json' })
    response.write('{"api_version":"st3.client.v0","snapshot":{},"value":')
    started.resolve()
  })
  await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve))
  const address = server.address()
  if (address === null || typeof address === 'string') throw new Error('No protocol server address')
  const registry = AtomRegistry.make()
  try {
    const reads = gatewaySubjectReads({
      options: { baseUrl: `http://127.0.0.1:${address.port}` },
      registry,
    })
    const stop = registry.subscribe(reads.machine.feed('machine/missing'), () => {}, {
      immediate: true,
    })
    await started.promise
    stop()
    await closed.promise
    await new Promise<void>((resolve) => setImmediate(resolve))
    expect(requests).toBe(1)
  } finally {
    registry.dispose()
    server.closeAllConnections()
    await new Promise<void>((resolve, reject) =>
      server.close((error) => (error ? reject(error) : resolve())),
    )
  }
})

it('never requests another page after a non-cooperative transport resolves following unmount', async () => {
  const firstPage = Promise.withResolvers<Response>()
  const started = Promise.withResolvers<void>()
  let requests = 0
  let requestSignal: AbortSignal | undefined
  const registry = AtomRegistry.make()
  try {
    const reads = gatewaySubjectReads({
      options: {
        baseUrl: 'http://gateway.invalid',
        fetchImpl: async (input, init) => {
          if (String(input).endsWith('/capabilities')) return Response.json(recording.capabilities)
          requests++
          requestSignal = init?.signal ?? undefined
          started.resolve()
          return firstPage.promise
        },
      },
      registry,
    })
    const stop = registry.subscribe(reads.machine.feed('machine/missing'), () => {}, {
      immediate: true,
    })
    await started.promise
    stop()
    await new Promise<void>((resolve) => setImmediate(resolve))
    expect(requestSignal?.aborted).toBe(true)
    firstPage.resolve(
      Response.json({
        api_version: 'st3.client.v0',
        snapshot: {},
        value: {
          kind: 'page',
          items: [],
          page: { has_more: true, next_cursor: 'next', limit: 100 },
        },
      }),
    )
    await new Promise<void>((resolve) => setImmediate(resolve))
    expect(requests).toBe(1)
  } finally {
    registry.dispose()
  }
})

it('shares one in-flight acquisition across feed, two streams and a typed read until the last consumer closes', async () => {
  const registry = AtomRegistry.make()
  const started = Promise.withResolvers<void>()
  let requests = 0
  let signal: AbortSignal | undefined
  const reads = gatewaySubjectReads({
    options: {
      baseUrl: 'http://gateway.invalid',
      fetchImpl: async (input, init) => {
        if (String(input).endsWith('/capabilities')) return Response.json(recording.capabilities)
        requests++
        signal = init?.signal ?? undefined
        started.resolve()
        return new Promise<Response>(() => {})
      },
    },
    registry,
  })
  const stop = registry.subscribe(reads.machine.feed('machine/shared'), () => {}, {
    immediate: true,
  })
  const first = Effect.runFork(Stream.runDrain(reads.machine.changes('machine/shared')))
  const second = Effect.runFork(Stream.runDrain(reads.machine.changes('machine/shared')))
  const typed = Effect.runFork(reads.machine.read('machine/shared'))
  try {
    await started.promise
    await new Promise<void>((resolve) => setImmediate(resolve))
    expect(requests).toBe(1)
    stop()
    await Effect.runPromise(Fiber.interrupt(first))
    await Effect.runPromise(Fiber.interrupt(typed))
    expect(signal?.aborted).toBe(false)
    await Effect.runPromise(Fiber.interrupt(second))
    await new Promise<void>((resolve) => setImmediate(resolve))
    expect(signal?.aborted).toBe(true)
    expect(requests).toBe(1)
  } finally {
    stop()
    await Effect.runPromise(Fiber.interrupt(first))
    await Effect.runPromise(Fiber.interrupt(second))
    await Effect.runPromise(Fiber.interrupt(typed))
    registry.dispose()
  }
})
