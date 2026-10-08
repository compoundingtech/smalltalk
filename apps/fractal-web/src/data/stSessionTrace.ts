import { St3Client } from '@smalltalk/st3-client'
import * as Native from '@smalltalk/st3-client/schema'
import { Context, DateTime, Duration, Effect, Layer, Option, Schema } from 'effect'

import {
  SessionTraceProvider,
  TraceFact,
  TraceFreshness,
  type TraceQuery,
  type TraceSeries,
  type TraceUnknown,
} from './sessionTrace.ts'
import { SubjectReadPort } from './subjectReadPort.ts'

export const StTraceFacts = Schema.Struct({
  usage: TraceFact(Schema.toType(Native.UsageSummary)),
  row: TraceFact(Schema.toType(Native.UsageRow)),
  freshness: TraceFreshness,
}).annotate({ identifier: 'SessionTrace.NativeFacts' })
export type StTraceFacts = typeof StTraceFacts.Type

/** Reuses the native roster summary and usage row; it does not create a second accounting source. */
export class StSessionTraceFacts extends Context.Service<StSessionTraceFacts, {
  readonly read: (query: TraceQuery) => Effect.Effect<StTraceFacts>
}>()('Fractal/StSessionTraceFacts') {}

const missing: TraceUnknown = { _tag: 'Unknown', reason: 'not-observed' }
const absentProvider: TraceUnknown = { _tag: 'Unknown', reason: 'no-provider' }
const known = <TValue>(value: TValue) => ({ _tag: 'Known' as const, value })

/** Roster totals are agent-incarnation, main-session-only meters, never range or root totals. */
export const stSessionTraceLayer = Layer.effect(SessionTraceProvider,
  Effect.gen(function* () {
    const facts = yield* StSessionTraceFacts
    return {
      load: Effect.fn('SessionTrace.st.load')((query: TraceQuery) =>
        Effect.gen(function* () {
          const observation = yield* facts.read(query)
          const usage = observation.usage
          const field = <TValue>(pick: (value: Native.UsageSummary) => TValue | undefined) => {
            if (usage._tag === 'Unknown') return usage
            const value = pick(usage.value)
            return value === undefined ? missing : known(value)
          }
          const context = usage._tag === 'Known' ? usage.value.context : undefined
          const contextField = <TValue>(value: TValue | undefined) =>
            usage._tag === 'Unknown' ? usage : value === undefined ? missing : known(value)
          const row = observation.row
          // A returned row must establish the requested join; never echo an unverified input as Known.
          const nativeSessionId = row._tag === 'Unknown' ? row
            : row.value.native_session_id === query.native_session_id ? known(query.native_session_id)
            : { _tag: 'Unknown' as const, reason: 'not-attributed' as const }
          return {
            query,
            native_session_id: nativeSessionId,
            scope: 'self',
            partial: true,
            freshness: observation.freshness,
            meters: {
              scope: 'agent-incarnations',
              partial: true,
              tokens: {
                input: field((value) => value.input_tokens),
                output: field((value) => value.output_tokens),
                cacheRead: field((value) => value.cached_tokens),
                cacheWrite: field((value) => value.cache_write_tokens),
              },
              cost: { amount: field((value) => value.cost), currency: field((value) => value.currency) },
              context: {
                usedTokens: contextField(context?.used_tokens),
                windowTokens: contextField(context?.window_tokens),
                usedPercent: contextField(context?.used_percent),
                compactions: contextField(context?.compactions),
                lastCompactionMs: contextField(context?.last_compaction_ms === undefined
                  ? undefined : Duration.toMillis(context.last_compaction_ms)),
                observedAtMs: contextField(context === undefined ? undefined : DateTime.toEpochMillis(context.observed_at_unix_ms)),
                model: contextField(context?.model),
              },
            },
            buckets: absentProvider,
            subagents: absentProvider,
          } satisfies TraceSeries
        }),
      ),
    }
  }),
)

const rangeMs = { '1d': 86_400_000, '7d': 604_800_000, '30d': 2_592_000_000 } as const

/** Native usage establishes the root join; the existing subject reader supplies the roster meters. */
export const gatewayStSessionTraceFactsLayer = (client: St3Client) => Layer.effect(StSessionTraceFacts,
  Effect.gen(function* () {
    const subjects = yield* SubjectReadPort
    return {
      read: Effect.fn('SessionTrace.st.read')((query: TraceQuery) =>
        Effect.gen(function* () {
          const now = yield* DateTime.now
          const until_ms = DateTime.toEpochMillis(now)
          const period = yield* Effect.tryPromise({
            try: () => client.usagePeriod({ since_ms: until_ms - rangeMs[query.range], until_ms }),
            catch: () => ({ _tag: 'Unknown' as const, reason: 'failed' as const, detail: 'Native usage read failed' }),
          }).pipe(Effect.flatMap((envelope) => Schema.decodeUnknownEffect(Native.UsagePeriod)(envelope.value)),
            Effect.catch(() => Effect.succeed(undefined)))
          if (period === undefined) {
            const failed: TraceUnknown = { _tag: 'Unknown', reason: 'failed', detail: 'Native usage is unavailable' }
            return { usage: failed, row: failed, freshness: failed }
          }
          const rows = period.rows.filter((row) => row.native_session_id === query.native_session_id)
          const row = rows[0]
          // No summation, path attribution or arbitrary choice between agents sharing an ID.
          if (row === undefined || rows.some((candidate) => candidate.agent !== row.agent)) {
            const unattributed: TraceUnknown = { _tag: 'Unknown', reason: 'not-attributed' }
            return { usage: unattributed, row: unattributed, freshness: { _tag: 'Observed' as const, atMs: until_ms } }
          }
          const agent = yield* subjects.agent.read(row.agent).pipe(
            Effect.map((agent) => ({ _tag: 'Known' as const, value: agent })),
            Effect.catch((error) => Effect.succeed({ _tag: 'Unknown' as const, reason: 'failed' as const, detail: error.detail })),
          )
          const usage = agent._tag === 'Unknown' ? agent : Option.match(agent.value.usage, {
            onNone: () => missing,
            onSome: known,
          })
          return { usage, row: known(row), freshness: { _tag: 'Observed' as const, atMs: until_ms } }
        }),
      ),
    }
  }),
)

export const gatewaySessionTraceLayer = (client: St3Client) => stSessionTraceLayer.pipe(
  Layer.provide(gatewayStSessionTraceFactsLayer(client)),
)
