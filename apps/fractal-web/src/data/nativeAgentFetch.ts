/**
 * Private web compatibility for native wildcard agent details. The SDK encodes the whole ref;
 * the same-origin edge admits raw path segments. This adapter changes only that GET read.
 */
import { Schema } from 'effect'

import { SubjectAddress } from '../resources/contract.ts'

const decodeAddress = Schema.decodeUnknownSync(SubjectAddress)
const encodedAgentDetail = /^\/v1\/client\/agents\/([^/]+)$/u

export const nativeAgentFetch = ({ origin, fetchImpl }: {
  readonly origin: string
  readonly fetchImpl: typeof globalThis.fetch
}): typeof globalThis.fetch => {
  const admittedOrigin = new URL(origin).origin
  return (input, init) => {
    const request = typeof Request !== 'undefined' && input instanceof Request ? input : undefined
    const method = (init?.method ?? request?.method ?? 'GET').toUpperCase()
    if (method !== 'GET') return fetchImpl(input, init)
    const url = new URL(request?.url ?? String(input), admittedOrigin)
    if (url.origin !== admittedOrigin) return fetchImpl(input, init)
    const match = encodedAgentDetail.exec(url.pathname)
    if (match === null || !/%(?:2f|25)/iu.test(match[1]!)) return fetchImpl(input, init)
    if (url.username !== '' || url.password !== '' || url.hash !== '')
      return Promise.reject(new TypeError('Agent detail URL contains credentials or a fragment'))
    let ref: string
    try {
      ref = decodeURIComponent(match[1]!)
      decodeAddress({ ref, presentation: 'detail' })
    } catch {
      return Promise.reject(new TypeError('Agent detail reference is not a canonical subject address'))
    }
    const segments = ref.split('/')
    if (segments[0] !== 'agent' || segments.length < 2 || segments.some((segment) =>
      segment === '' || segment === '.' || segment === '..' || /[%\\?#\p{Cc}\p{Cf}]/u.test(segment)))
      return Promise.reject(new TypeError('Agent detail reference contains an unsafe path segment'))
    url.pathname = `/v1/client/agents/${segments.map(encodeURIComponent).join('/')}`
    // Request cloning preserves its headers, credentials, mode, signal and other fetch policy;
    // init remains the caller's original override object. Strings/URLs keep init by identity.
    return fetchImpl(request === undefined ? url.href : new Request(url.href, request), init)
  }
}
