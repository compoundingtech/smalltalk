import { expect, it } from 'vitest'
import type { IncomingHttpHeaders } from 'node:http'
import { cleanHeaders, withoutTraceQuery } from './gateway.mts'
import { incomingParent, requestSpanOptions, traceHeaders } from './tracing.mts'

const traceId = '4bf92f3577b34da6a3ce929d0e0e4736'
const spanId = '00f067aa0ba902b7'
const browserParent = `00-${traceId}-${spanId}-01`
const request = (headers: IncomingHttpHeaders = {}, url = '/v1/client/collections/stream') => ({ headers, url, method: 'GET' })

it.each([
  { flags: '00', sampled: false }, { flags: '01', sampled: true },
  { flags: '02', sampled: false }, { flags: '03', sampled: true },
])('accepts version-00 context and uses only the sampled bit ($flags)', ({ flags, sampled }) => {
  const parent = incomingParent(request({ traceparent: `00-${traceId}-${spanId}-${flags}` }))
  expect(parent).toEqual(expect.objectContaining({ _tag: 'ExternalSpan', traceId, spanId, sampled }))
  expect(requestSpanOptions(request({ traceparent: browserParent })).root).toBe(false)
})

it.each([
  { name: 'missing', header: undefined },
  { name: 'empty', header: '' },
  { name: 'zero trace', header: `00-${'0'.repeat(32)}-${spanId}-01` },
  { name: 'zero parent', header: `00-${traceId}-${'0'.repeat(16)}-01` },
  { name: 'uppercase', header: browserParent.toUpperCase() },
  { name: 'nonhex', header: `00-${'g'.repeat(32)}-${spanId}-01` },
  { name: 'truncated', header: browserParent.slice(0, -1) },
  { name: 'extra field', header: `${browserParent}-extra` },
  { name: 'trailing newline', header: `${browserParent}\n` },
  { name: 'whitespace', header: ` ${browserParent}` },
  { name: 'unsupported version', header: browserParent.replace(/^00/, '01') },
  { name: 'forbidden version', header: browserParent.replace(/^00/, 'ff') },
  { name: 'joined duplicate headers', header: `${browserParent},${browserParent}` },
  { name: 'array headers', header: [browserParent, browserParent] },
])('discards invalid context: $name', ({ header }) => {
  const req = request({ traceparent: header, tracestate: 'fixture=browser' })
  expect(incomingParent(req)).toBeUndefined()
  expect(requestSpanOptions(req)).toEqual(expect.objectContaining({ parent: undefined, root: true }))
})

it('preserves validated vendor state as propagation context, not telemetry attributes', () => {
  const tracestate = 'fixture=browser,1tenant@vendor=opaque value'
  const parent = incomingParent(request({ traceparent: browserParent, tracestate }))
  if (parent === undefined) throw new TypeError('Expected a validated parent')
  expect(traceHeaders(parent)).toEqual({ traceparent: browserParent, tracestate })
  expect(requestSpanOptions(request({ traceparent: browserParent, tracestate })).attributes).toEqual({
    'span.label': '/v1/client/*', 'http.route': '/v1/client/*', 'http.request.method': 'GET',
  })
})

it.each([
  { name: 'empty', state: '' },
  { name: 'empty member', state: 'fixture=browser,,vendor=value' },
  { name: 'duplicate key', state: 'fixture=browser,fixture=duplicate' },
  { name: 'uppercase key', state: 'Fixture=browser' },
  { name: 'leading digit in simple key', state: '1fixture=browser' },
  { name: 'invalid tenant system', state: '1tenant@1system=value' },
  { name: 'overlong simple key', state: `${'k'.repeat(257)}=value` },
  { name: 'overlong tenant', state: `${'t'.repeat(242)}@vendor=value` },
  { name: 'overlong system', state: `tenant@${'s'.repeat(15)}=value` },
  { name: 'empty value', state: 'fixture=' },
  { name: 'embedded equals', state: 'fixture=not=valid' },
  { name: 'embedded tab', state: 'fixture=not\tvalid' },
  { name: 'nonascii', state: 'fixture=café' },
  { name: 'header injection', state: 'fixture=value\r\nx-private: injected' },
  { name: 'trailing newline', state: 'fixture=value\n' },
  { name: 'overlong value', state: `fixture=${'v'.repeat(257)}` },
  { name: 'overlong header', state: `a=${'v'.repeat(256)},b=${'v'.repeat(256)}` },
  { name: 'too many members', state: Array.from({ length: 33 }, (_, index) => `v${index}=x`).join(',') },
  { name: 'array headers', state: ['fixture=browser'] },
])('drops invalid tracestate without losing the valid parent: $name', ({ state }) => {
  const parent = incomingParent(request({ traceparent: browserParent, tracestate: state }))
  if (parent === undefined) throw new TypeError('Valid traceparent must survive invalid tracestate')
  expect(traceHeaders(parent)).toEqual({ traceparent: browserParent })
})

it('accepts bounded tracestate with optional list whitespace and maximum member count', () => {
  for (const tracestate of [' \tfixture=browser \t, vendor=next ', Array.from({ length: 32 }, (_, index) => `v${index}=x`).join(',')]) {
    const parent = incomingParent(request({ traceparent: browserParent, tracestate }))
    if (parent === undefined) throw new TypeError('Expected a validated parent')
    expect(traceHeaders(parent)).toEqual({ traceparent: browserParent, tracestate })
  }
})

it('accepts upgrade query context only at the explicit upgrade boundary', () => {
  const req = request({}, `/v1/client/collections/stream?traceparent=${browserParent}&tracestate=fixture%3Dbrowser`)
  expect(incomingParent(req)).toBeUndefined()
  const parent = incomingParent(req, true)
  if (parent === undefined) throw new TypeError('Expected upgrade query parent')
  expect(traceHeaders(parent)).toEqual({ traceparent: browserParent, tracestate: 'fixture=browser' })
  expect(requestSpanOptions(req, true).root).toBe(false)
})
it('preserves a literal question mark inside a validated query state value', () => {
  const parent = incomingParent(request({}, `/v1/client/collections/stream?traceparent=${browserParent}&tracestate=fixture%3Dvalue?tail`), true)
  if (parent === undefined) throw new TypeError('Expected query parent')
  expect(traceHeaders(parent)).toEqual({ traceparent: browserParent, tracestate: 'fixture=value?tail' })
})

it('rejects duplicate query parents and prefers headers without falling back from invalid headers', () => {
  const url = `/v1/client/collections/stream?traceparent=${browserParent}`
  expect(incomingParent(request({}, `${url}&traceparent=${browserParent}`), true)).toBeUndefined()
  expect(incomingParent(request({ traceparent: 'invalid' }, url), true)).toBeUndefined()
  const headerParent = `00-${traceId}-${'1'.repeat(16)}-00`
  const parent = incomingParent(request({ traceparent: headerParent, tracestate: 'fixture=header' }, `${url}&tracestate=fixture%3Dquery`), true)
  if (parent === undefined) throw new TypeError('Expected header precedence')
  expect(traceHeaders(parent)).toEqual({ traceparent: headerParent, tracestate: 'fixture=header' })
})

it('discards duplicate and header-injecting query state while retaining the valid upgrade parent', () => {
  for (const state of ['tracestate=fixture%3Dbrowser&tracestate=vendor%3Dduplicate', 'tracestate=fixture%3Dx%0D%0AX-Private%3Ainjected']) {
    const parent = incomingParent(request({}, `/v1/client/collections/stream?traceparent=${browserParent}&${state}`), true)
    if (parent === undefined) throw new TypeError('Valid query parent must survive invalid state')
    expect(traceHeaders(parent)).toEqual({ traceparent: browserParent })
  }
})

it('strips all upgrade propagation fields while preserving unrelated query bytes', () => {
  expect(withoutTraceQuery(`/v1/client/collections/stream?cursor=a%2fb+%20&traceparent=${browserParent}&trace%73tate=private&traceparent=duplicate&kind=agent`))
    .toBe('/v1/client/collections/stream?cursor=a%2fb+%20&kind=agent')
  expect(withoutTraceQuery(`/v1/client/collections/stream?traceparent=${browserParent}&tracestate=private`)).toBe('/v1/client/collections/stream')
  expect(withoutTraceQuery('/v1/client/collections/stream?cursor=%ZZ')).toBe('/v1/client/collections/stream?cursor=%ZZ')
  expect(withoutTraceQuery(undefined)).toBeUndefined()
  expect(cleanHeaders({ traceparent: browserParent, tracestate: 'not validated', baggage: 'private=value', origin: 'https://example.test', cookie: 'private=value' })).toEqual({})
})
