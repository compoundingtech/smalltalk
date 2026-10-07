import type { ClientOptions } from '@smalltalk/st3-client'
import { Agent } from '@smalltalk/st3-client/schema'
import { DateTime, Effect, Schema, Stream } from 'effect'
import { describe, expect, it } from 'vitest'

import { observed, unavailable } from './source.ts'
import recording from './subjectReadPort.gateway.fixtures.json' with { type: 'json' }
import {
  fixtureSubjectReads,
  gatewaySubjectReads,
  nativeSubjectFixture,
} from './subjectReadPort.ts'

const envelope = (value: unknown) => ({
  api_version: 'st3.client.v0',
  snapshot: { id: 'snapshot/test' },
  value,
})

describe('native subject reads', () => {
  it('decodes fixture wire once before publishing rich values to typed registry consumers', () => {
    const feed = nativeSubjectFixture({ schema: Agent, wire: recording.cases.agent.payload.value })
    expect(feed._tag).toBe('Observed')
    if (feed._tag !== 'Observed') return
    expect(DateTime.isDateTime(feed.value.updated_at)).toBe(true)
    const typed = Schema.decodeSync(Schema.toType(Agent))(feed.value)
    expect(typed.id).toBe(recording.cases.agent.ref)
    expect(() =>
      Schema.decodeUnknownSync(Schema.toType(Agent))({
        ...feed.value,
        updated_at: 'not-a-rich-date',
      }),
    ).toThrow()
  })

  it('preserves unrelated fixtures when overriding one subject ref', async () => {
    const reads = fixtureSubjectReads({
      fixtures: {
        document: {
          'doc/a': observed({ value: { reference: 'blob/a', bytes: [] } }),
          'doc/b': observed({ value: { reference: 'blob/b', bytes: [66] } }),
        },
      },
      overrides: {
        document: { 'doc/a': unavailable({ reason: 'ungranted', detail: 'Read grant denied' }) },
      },
    })
    expect(await Effect.runPromise(reads.document.read('doc/b'))).toEqual({
      reference: 'blob/b',
      bytes: [66],
    })
    expect(await Effect.runPromise(Effect.flip(reads.document.read('doc/a')))).toMatchObject({
      reason: 'ungranted',
      detail: 'Read grant denied',
    })
  })

  it('keeps the last good native DTO with an explicit stale failure then recovers', async () => {
    let failed = false
    const client: ClientOptions = {
      baseUrl: 'http://gateway.invalid',
      fetchImpl: async (input) => {
        if (String(input).endsWith('/capabilities'))
          return Response.json(envelope({ limits: { max_page_items: 100 } }))
        if (failed) throw new Error('Gateway disconnected')
        return Response.json(envelope({ reference: 'blob/document', bytes: [65] }))
      },
    }
    const reader = gatewaySubjectReads({ options: client }).document
    const one = () =>
      Effect.runPromise(
        Stream.runCollect(
          reader.changes('guide').pipe(
            Stream.filter(
              (state) =>
                state._tag === 'Unavailable' ||
                (state._tag === 'Observed' &&
                  (state.freshness === 'live' || state.error !== undefined)),
            ),
            Stream.take(1),
          ),
        ),
      )
    expect(Array.from(await one())[0]).toEqual(
      observed({ value: { reference: 'blob/document', bytes: [65] } }),
    )
    failed = true
    expect(Array.from(await one())[0]).toEqual({
      _tag: 'Observed',
      value: { reference: 'blob/document', bytes: [65] },
      freshness: 'stale',
      error: { reason: 'failed', detail: 'Gateway disconnected' },
    })
    failed = false
    expect(Array.from(await one())[0]).toEqual(
      observed({ value: { reference: 'blob/document', bytes: [65] } }),
    )
  })

  it('decodes the selected machine independently of unrelated malformed list rows', async () => {
    const client: ClientOptions = {
      baseUrl: 'http://gateway.invalid',
      fetchImpl: async (input) => {
        if (String(input).endsWith('/capabilities'))
          return Response.json(envelope({ limits: { max_page_items: 100 } }))
        return Response.json(
          envelope({
            kind: 'page',
            items: [
              {
                id: 'machine/selected',
                kind: 'machine',
                revision: '1',
                updated_at: '2026-10-05T00:00:00Z',
                host_id: 'host/selected',
                name: 'Recorded machine',
                state: 'local',
                capacity: { state: 'unknown', reason: 'no capacity observation' },
                occupancy: { running_runtimes: 16 },
                transports: [],
                projects: [],
                work: [],
                runtime_ids: [],
              },
              { id: 'machine/other', kind: 'machine', state: 'unknown-new-state' },
            ],
            page: { has_more: false, limit: 100 },
          }),
        )
      },
    }
    const result = await Effect.runPromise(
      gatewaySubjectReads({ options: client }).machine.read('machine/selected'),
    )
    expect(result).toMatchObject({
      id: 'machine/selected',
      state: 'local',
      host_id: 'host/selected',
    })
  })
})
