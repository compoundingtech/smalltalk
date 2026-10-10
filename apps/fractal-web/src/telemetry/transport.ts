import * as Atom from 'effect/reactivity/Atom'
import { wrapTraceFetch, type St3Options } from '@st3/sdk/effect'

import { getDebug, incrDebug, setDebug } from './meters.tsx'

/** Observe fetches and propagate UX context only to the admitted same-origin data boundary. */
export const instrumentFetch = ({ fetchImpl, origin, traceContext }: {
  readonly fetchImpl: typeof globalThis.fetch
  readonly origin: string
  readonly traceContext?: St3Options['traceContext']
}): typeof globalThis.fetch => {
  const fetch = traceContext === undefined ? fetchImpl : wrapTraceFetch({ fetchImpl, traceContext, baseUrl: origin })
  const samples: number[] = []
  let cursor = 0
  let inFlight = 0
  setDebug('Wf.httpInFlight', 0)
  setDebug('Wf.httpErrors', 0)
  setDebug('Wf.httpP50Ms', 0)
  setDebug('Wf.httpP95Ms', 0)
  return async (...[input, init]) => {
    const started = performance.now()
    setDebug('Wf.httpInFlight', ++inFlight)
    try {
      const response = await fetch(input, init)
      if (!response.ok) incrDebug('Wf.httpErrors')
      return response
    } catch (error) {
      // User cancellation is not a failed transport request.
      if (!(error instanceof Error && error.name === 'AbortError')) incrDebug('Wf.httpErrors')
      throw error
    } finally {
      setDebug('Wf.httpInFlight', --inFlight)
      samples[cursor] = performance.now() - started
      cursor = (cursor + 1) % 256
      const sorted = samples.toSorted((a, b) => a - b)
      setDebug('Wf.httpP50Ms', sorted[Math.ceil(sorted.length * 0.5) - 1]!)
      setDebug('Wf.httpP95Ms', sorted[Math.ceil(sorted.length * 0.95) - 1]!)
    }
  }
}

/** Sampled SDK transport health and activity reported by the debug bag. */
export interface TransportSnapshot {
  readonly socketLive: boolean
  readonly requests: number
  readonly p50Ms: number
  readonly p95Ms: number
  readonly errors: number
  readonly subscriptions: number
  readonly subscriptionCap: number
  readonly messagesPerSecond: number
}
/** A sampling view of DebugBag, never a second telemetry writer; only this row subscribes. */
export const transportSnapshot = Atom.make((get): TransportSnapshot => {
  let previousFrames = getDebug('Wf.frames')
  let previousAt = performance.now()
  const sample = (): TransportSnapshot => {
    const now = performance.now()
    const frames = getDebug('Wf.frames')
    const messagesPerSecond =
      now === previousAt ? 0 : (Math.max(0, frames - previousFrames) * 1000) / (now - previousAt)
    previousFrames = frames
    previousAt = now
    return {
      socketLive: getDebug('Wf.socketLive') === 1,
      requests: getDebug('Wf.httpInFlight'),
      p50Ms: getDebug('Wf.httpP50Ms'),
      p95Ms: getDebug('Wf.httpP95Ms'),
      errors: getDebug('Wf.httpErrors'),
      subscriptions: getDebug('Wf.activeFollows'),
      subscriptionCap: getDebug('Wf.followCap'),
      messagesPerSecond,
    }
  }
  const timer = setInterval(() => get.setSelf(sample()), 500)
  get.addFinalizer(() => clearInterval(timer))
  return sample()
}).pipe(
  Atom.withEquality(
    (a: TransportSnapshot, b: TransportSnapshot) =>
      a.socketLive === b.socketLive &&
      a.requests === b.requests &&
      a.p50Ms === b.p50Ms &&
      a.p95Ms === b.p95Ms &&
      a.errors === b.errors &&
      a.subscriptions === b.subscriptions &&
      a.subscriptionCap === b.subscriptionCap &&
      a.messagesPerSecond === b.messagesPerSecond,
  ),
)
