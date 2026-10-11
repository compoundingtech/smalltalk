import { Context, Effect, Schema } from 'effect'

const Count = Schema.Int.check(Schema.isGreaterThanOrEqualTo(0))
const Amount = Schema.Finite.check(Schema.isGreaterThanOrEqualTo(0))
const EpochMs = Count
const SessionId = Schema.NonEmptyString

export const TraceUnknown = Schema.TaggedStruct('Unknown', {
  reason: Schema.Literals(['no-provider', 'not-covered', 'not-attributed', 'out-of-retention', 'loading', 'failed', 'not-observed']),
  detail: Schema.optionalKey(Schema.String),
}).annotate({ identifier: 'SessionTrace.Unknown' })
export type TraceUnknown = typeof TraceUnknown.Type

/** Absence is not zero. Provider names are not part of the public wire contract. */
export const TraceFact = <TValue extends Schema.Top>(value: TValue) => Schema.Union([
  Schema.TaggedStruct('Known', { value }),
  TraceUnknown,
])

export const TraceQuery = Schema.Struct({
  native_session_id: SessionId,
  range: Schema.Literals(['1d', '7d', '30d']),
  bucket: Schema.Literals(['5m', '15m', '1h', '1d']),
}).annotate({ identifier: 'SessionTrace.Query' })
export type TraceQuery = typeof TraceQuery.Type

export const TraceFreshness = Schema.Union([
  Schema.TaggedStruct('Observed', { atMs: EpochMs }),
  Schema.TaggedStruct('Stale', { atMs: TraceFact(EpochMs), detail: Schema.String }),
  TraceUnknown,
]).annotate({ identifier: 'SessionTrace.Freshness' })
export type TraceFreshness = typeof TraceFreshness.Type

const Tokens = Schema.Struct({
  input: TraceFact(Count),
  output: TraceFact(Count),
  cacheRead: TraceFact(Count),
  cacheWrite: TraceFact(Count),
})
const Cost = Schema.Struct({ amount: TraceFact(Amount), currency: TraceFact(Schema.NonEmptyString) })
const Bucket = Schema.Struct({
  t: TraceFact(EpochMs),
  tokens: Tokens,
  cost: Cost,
  model: TraceFact(Schema.NonEmptyString),
})
const Subagent = Schema.Struct({
  native_session_id: TraceFact(SessionId),
  parent_session_id: TraceFact(SessionId),
  model: TraceFact(Schema.NonEmptyString),
  tokens: Tokens,
  cost: Cost,
})

/** The complete trace response: one schema for meters, bucketed series and subagent breakdown. */
export const TraceSeries = Schema.Struct({
  query: TraceQuery,
  native_session_id: TraceFact(SessionId),
  scope: Schema.Literals(['self', 'including_subagents']),
  partial: Schema.Boolean,
  freshness: TraceFreshness,
  meters: Schema.Struct({
    // Native roster totals span agent incarnations, not the requested range or one root.
    scope: Schema.Literals(['agent-incarnations', 'self', 'including_subagents']),
    partial: Schema.Boolean,
    tokens: Tokens,
    cost: Cost,
    context: Schema.Struct({
      usedTokens: TraceFact(Count),
      windowTokens: TraceFact(Count),
      usedPercent: TraceFact(Amount),
      compactions: TraceFact(Count),
      lastCompactionMs: TraceFact(EpochMs),
      observedAtMs: TraceFact(EpochMs),
      model: TraceFact(Schema.NonEmptyString),
    }),
  }),
  buckets: TraceFact(Schema.Array(Bucket)),
  subagents: TraceFact(Schema.Array(Subagent)),
}).annotate({ identifier: 'SessionTrace.Series' })
export type TraceSeries = typeof TraceSeries.Type

export interface SessionTraceProviderService {
  /** Missing coverage is represented by Unknown fields; freshness is observation evidence, not liveness. */
  readonly load: (query: TraceQuery) => Effect.Effect<TraceSeries>
}

export class SessionTraceProvider extends Context.Service<SessionTraceProvider, SessionTraceProviderService>()(
  'Fractal/SessionTraceProvider',
) {}

/** Consumer layer example: provide either the native default or an independent provider. */
export const sessionTrace = Effect.fn('SessionTrace.load')((query: TraceQuery) =>
  Effect.gen(function* () {
    const provider = yield* SessionTraceProvider
    return yield* provider.load(query)
  }),
)
