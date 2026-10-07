import type { ClientOptions } from '@smalltalk/st3-client'
import { DateTime, Effect } from 'effect'
import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import { describe, expect, it } from 'vitest'

import { gatewayResources } from '../resources/agent/source.ts'
import recording from './subjectReadPort.gateway.fixtures.json' with { type: 'json' }
import {
  gatewaySubjectReads,
  subjectReaderFromAtom,
  type SubjectReader,
  type SubjectReads,
} from './subjectReadPort.ts'

interface Recording {
  readonly ref: string
  readonly status: number
  readonly payload: unknown
}

/** Real GET responses, privacy-redacted without replacing enum/state/time/numeric contracts. */
const contract = <T>(
  family: string,
  fixture: Recording,
  select: (reads: SubjectReads) => SubjectReader<T>,
) => {
  it(`${family}: decodes the recorded response or preserves its real refusal`, async () => {
    let corrupt = false
    const client: ClientOptions = {
      baseUrl: 'http://recorded-gateway.invalid',
      fetchImpl: async (input) => {
        if (String(input).endsWith('/capabilities')) return Response.json(recording.capabilities)
        const payload: unknown = structuredClone(fixture.payload)
        if (corrupt && typeof payload === 'object' && payload !== null && 'value' in payload) {
          const value = payload.value
          if (typeof value === 'object' && value !== null) {
            if ('items' in value && Array.isArray(value.items)) {
              for (const row of value.items)
                if (row.id === fixture.ref) row.kind = 'invalid-native-kind'
            } else if ('kind' in value) value.kind = 'invalid-native-kind'
          }
        }
        return Response.json(payload, { status: fixture.status })
      },
    }
    const reader = select(gatewaySubjectReads({ options: client }))
    const result = await Effect.runPromise(Effect.result(reader.read(fixture.ref)))
    if (fixture.status !== 200 || family.startsWith('launch-')) {
      expect(result._tag).toBe('Failure')
      if (result._tag === 'Failure')
        expect(result.failure.reason).toBe(fixture.status === 403 ? 'ungranted' : 'failed')
      return
    }
    expect(result._tag).toBe('Success')
    if (
      result._tag === 'Success' &&
      typeof result.success === 'object' &&
      result.success !== null
    ) {
      if ('id' in result.success) expect(result.success.id).toBe(fixture.ref)
      if ('updated_at' in result.success)
        expect(DateTime.isDateTime(result.success.updated_at)).toBe(true)
    }
    corrupt = true
    expect(await Effect.runPromise(Effect.flip(reader.read(fixture.ref)))).toMatchObject({
      _tag: 'SubjectReadFailure',
      reason: 'failed',
    })
  })
}

describe('recorded native gateway contracts', () => {
  contract('launch', recording.cases.launch, (reads) => reads.launch)
  contract('launch-variant', recording.cases.launchVariant, (reads) => reads.launchVariant)
  contract('launch-decision', recording.cases.launchDecision, (reads) => reads.launchDecision)
  contract('launch-approval', recording.cases.launchApproval, (reads) => reads.launchApproval)
  contract('mission', recording.cases.mission, (reads) => reads.mission)
  contract('work', recording.cases.work, (reads) => reads.work)
  contract('lane', recording.cases.lane, (reads) => reads.lane)
  contract('agent-queue', recording.cases.agentQueue, (reads) => reads.agentQueue)
  contract('agent', recording.cases.agent, (reads) => reads.agent)
  contract('attention', recording.cases.attention, (reads) => reads.attention)
  contract('message', recording.cases.message, (reads) => reads.message)
  contract('runtime', recording.cases.runtime, (reads) => reads.runtime)
  contract('observer', recording.cases.observer, (reads) => reads.observer)
  contract('subscription', recording.cases.subscription, (reads) => reads.subscription)
  contract('operation', recording.cases.operation, (reads) => reads.operation)
  contract('history', recording.cases.history, (reads) => reads.history)
  contract('session', recording.cases.session, (reads) => reads.session)
  contract('terminal-screen', recording.cases.terminalScreen, (reads) => reads.terminalScreen)
  contract('document', recording.cases.document, (reads) => reads.document)
  contract('blob', recording.cases.blob, (reads) => reads.blob)
  contract('glass', recording.cases.glass, (reads) => reads.glass)
  contract('device', recording.cases.device, (reads) => reads.device)
  contract('machine', recording.cases.machine, (reads) => reads.machine)
  it('mission-run: resolves an opaque run from authoritative recorded mission detail', async () => {
    const fixture = recording.cases.missionRun
    const reads = gatewaySubjectReads({
      options: {
        baseUrl: 'http://recorded-gateway.invalid',
        fetchImpl: async (input) => {
          if (String(input).endsWith('/capabilities')) return Response.json(recording.capabilities)
          if (new URL(String(input)).pathname === '/v1/client/missions')
            return Response.json({
              ...fixture.payload,
              value: {
                kind: 'page',
                items: [fixture.payload.value],
                page: { has_more: false, limit: 100 },
              },
            })
          return Response.json(fixture.payload)
        },
      },
    })
    const run = await Effect.runPromise(reads.missionRun.read(fixture.ref))
    expect(run.id).toBe(fixture.ref)
    expect(DateTime.isDateTime(run.state_since)).toBe(true)
    expect(Object.keys(fixture.payload.value.run_generations)).toContain(run.id)
  })

  it('resource: preserves the recorded rich observation through native and graph readers', async () => {
    const fixture = recording.cases.resource
    const registry = AtomRegistry.make()
    const source = gatewayResources({
      baseUrl: 'http://recorded-gateway.invalid',
      fetchImpl: async (input) =>
        Response.json(
          String(input).endsWith('/capabilities') ? recording.capabilities : fixture.payload,
        ),
    })
    try {
      const reader = subjectReaderFromAtom({ feed: source.byId, registry })
      const native = await Effect.runPromise(reader.read(fixture.ref))
      expect(native.id).toBe(fixture.ref)
      expect(DateTime.isDateTime(native.observed_at)).toBe(true)
      const graph = registry.get(source.byId(fixture.ref))
      expect(graph._tag).toBe('Observed')
      if (graph._tag === 'Observed') {
        expect(graph.value.observed_at).toEqual(native.observed_at)
        expect(graph.value.facts).toEqual(fixture.payload.value.items[0]?.facts)
      }
    } finally {
      registry.dispose()
    }
  })
})
