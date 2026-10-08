/**
 * Shared rebase vectors (spec "Shared vectors"). Expected values are written out by hand, not
 * computed by the kit, so the TypeScript, Rust and Swift readers are checked against one oracle.
 */
import type { TimePointer } from '../src/kit/time.ts'

export interface RebaseVector {
  readonly name: string
  readonly input: unknown
  readonly times: readonly TimePointer[]
  readonly anchor: string
  /** Epoch milliseconds. */
  readonly now: number
  /** The rebased document, or the pointer a reader must name in its range error. */
  readonly expected: { readonly _tag: 'ok'; readonly value: unknown } | { readonly _tag: 'range-error'; readonly pointer: string }
}

const anchor = '2030-01-01T12:00:00.000Z'
const ts = (pointer: string): TimePointer => ({ pointer, codec: 'timestamp' })

export const rebaseVectors: readonly RebaseVector[] = [
  {
    name: 'second, millisecond and microsecond inputs; a now with nonzero milliseconds',
    input: {
      second: '2030-01-01T11:59:00Z',
      milli: '2030-01-01T11:59:00.250Z',
      micro_a: '2030-01-01T11:59:00.250999Z',
      micro_b: '2030-01-01T11:59:00.250001Z',
      offset: '2030-01-01T13:00:00+01:00',
    },
    times: [ts('/second'), ts('/milli'), ts('/micro_a'), ts('/micro_b'), ts('/offset')],
    anchor,
    now: 1893585601500, // 2030-01-02T12:00:01.500Z
    expected: {
      _tag: 'ok',
      value: {
        second: '2030-01-02T11:59:01.500Z',
        milli: '2030-01-02T11:59:01.750Z',
        micro_a: '2030-01-02T11:59:01.750Z',
        micro_b: '2030-01-02T11:59:01.750Z',
        offset: '2030-01-02T12:00:01.500Z',
      },
    },
  },
  {
    name: 'epoch-ms integers move; durations, content and ids equal to a timestamp do not',
    input: {
      observed_at_unix_ms: 1893499140000,
      timeout_ms: 60000,
      text: '2030-01-01T11:59:00.000Z',
      id: 'message/scenario-2030-01-01T11:59:00.000Z',
      list: [{ 'a/b': '2030-01-01T11:00:00.000Z' }],
    },
    times: [{ pointer: '/observed_at_unix_ms', codec: 'epoch-ms' }, ts('/list/0/a~1b')],
    anchor,
    now: 1893495600000, // 2030-01-01T11:00:00.000Z
    expected: {
      _tag: 'ok',
      value: {
        observed_at_unix_ms: 1893495540000,
        timeout_ms: 60000,
        text: '2030-01-01T11:59:00.000Z',
        id: 'message/scenario-2030-01-01T11:59:00.000Z',
        list: [{ 'a/b': '2030-01-01T10:00:00.000Z' }],
      },
    },
  },
  {
    name: 'now equal to the anchor leaves values unchanged',
    input: { at: '2030-01-01T11:00:00.000Z' },
    times: [ts('/at')],
    anchor,
    now: 1893499200000,
    expected: { _tag: 'ok', value: { at: '2030-01-01T11:00:00.000Z' } },
  },
  {
    name: 'a shift past 9999-12-31 is rejected naming the pointer',
    input: { ok: '2030-01-01T11:00:00.000Z', late: '9999-12-31T00:00:00.000Z' },
    times: [ts('/ok'), ts('/late')],
    anchor,
    now: 1893672000000, // 2030-01-03T12:00:00.000Z
    expected: { _tag: 'range-error', pointer: '/late' },
  },
  {
    name: 'a shift before 0001-01-01 is rejected naming the pointer',
    input: { early: '0001-01-01T00:00:00.000Z' },
    times: [ts('/early')],
    anchor,
    now: 1893499199999,
    expected: { _tag: 'range-error', pointer: '/early' },
  },
]
