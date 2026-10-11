// moves to smalltalk/clients/typescript
/**
 * `SubjectEnvelope`: the one typed wrapper client-v0 would serve every subject in (q2).
 *
 * Every subject — mission, agent, pty, attention, `resource/*` fact bags — arrives as
 * `{ ref, family, schema, revision, observed_at, provenance, data, actions, live, partial? }`.
 * Clients route on `schema` (`st3.<family>[:<kind>]@<major>`); `data` is decoded only by the
 * renderer that claims it, so unknown schemas still decode as a header plus open fact bag.
 *
 * Conventions follow the generated client-v0 Effect schemas so this can be emitted by
 * `st3-client-codegen` later: snake_case wire names verbatim, `Schema.Struct` (never Class),
 * `optionalKey` for optional, `NullOr` for nullable, `Literals` for enums, ids and timestamps
 * as pattern-checked strings (no brands, no Date decoding). Domain numbers use `Finite`:
 * counts, accounting, dimensions and subscription parameters reject NaN and infinities.
 */
import { Result, Schema } from 'effect'

/** Subject reference: `family/rest`, e.g. `mission/m-7f2`, `resource/github/o/r/pull/403`. */
export const SubjectRef = Schema.String.check(Schema.isPattern(/^[a-z][a-z0-9-]*\/\S+$/u)).annotate(
  {
    identifier: 'SubjectRef',
  },
)
export type SubjectRef = typeof SubjectRef.Type

/** RFC 3339 timestamp, kept as a string on the wire. */
export const Timestamp = Schema.String.check(
  Schema.isPattern(/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})$/u),
).annotate({ identifier: 'Timestamp' })
export type Timestamp = typeof Timestamp.Type

/** Opaque per-subject revision (claim-log derived). */
export const Revision = Schema.String.annotate({ identifier: 'Revision' })

/** Subject family: the prefix of `ref` (`mission`, `agent`, `pty`, `attention`, `resource`, …). */
export const Family = Schema.String.check(Schema.isPattern(/^[a-z][a-z0-9-]*$/u)).annotate({
  identifier: 'Family',
})

const schemaIdPattern = /^st3\.([a-z][a-z0-9-]*)(?::([a-z0-9][a-z0-9.-]*))?@([1-9]\d*)$/u

/**
 * Schema id: `st3.<family>[:<kind>]@<major>`. The kind segment exists only for families whose
 * subjects carry a kind (`resource:vcs.pull-request`, `resource:custom.acme.board`). Only the
 * major is on the wire: additive changes are minors that decoders absorb (excess keys ignored,
 * new keys optional), so they never change routing.
 */
export const SchemaId = Schema.String.check(Schema.isPattern(schemaIdPattern)).annotate({
  identifier: 'SchemaId',
})
export type SchemaId = typeof SchemaId.Type

/** A schema id split into its routing parts. */
export interface ParsedSchemaId {
  /** Version-less name, e.g. `st3.resource:vcs.pull-request`. */
  readonly name: string
  readonly family: string
  readonly kind: string | undefined
  readonly major: number
}

/** Splits a schema id; `undefined` for strings that are not schema ids. */
export const parseSchemaId = (id: string): ParsedSchemaId | undefined => {
  const match = schemaIdPattern.exec(id)
  if (match === null) return undefined
  const [, family, kind, major] = match
  if (family === undefined || major === undefined) return undefined
  return {
    name: id.slice(0, id.lastIndexOf('@')),
    family,
    kind,
    major: Number(major),
  }
}

/** client-v0 `Fence`, verbatim: the optimistic-concurrency token an action must carry. */
export const Fence = Schema.Struct({
  snapshot_id: Schema.String,
  subject_revisions: Schema.Record(Schema.String, Revision),
  attempt: Schema.optionalKey(Schema.Finite),
  mission_generation: Schema.optionalKey(Schema.String),
  preview_token: Schema.optionalKey(Schema.String),
  readiness_epoch: Schema.optionalKey(Schema.Finite),
  runtime_desired_revision: Schema.optionalKey(Schema.String),
  runtime_incarnation: Schema.optionalKey(Schema.String),
  step_definition: Schema.optionalKey(Revision),
  terminal_sequence: Schema.optionalKey(Schema.Finite),
}).annotate({ identifier: 'Fence' })
export type Fence = typeof Fence.Type

/**
 * One action the session actor may take on this subject, pre-fenced at the envelope's snapshot.
 * `id` is the client-v0 action `type` (`mission.cancel`, `runtime.stop`, …). Offers the actor
 * lacks scope for are omitted, not listed as disabled (see open question "permissions").
 */
export const ActionOffer = Schema.Struct({
  id: Schema.String,
  label: Schema.String,
  fence: Fence,
  enabled: Schema.Boolean,
  disabled_reason: Schema.optionalKey(Schema.String),
  risk: Schema.Literals(['safe', 'confirm', 'destructive']),
}).annotate({ identifier: 'ActionOffer' })
export type ActionOffer = typeof ActionOffer.Type

/**
 * Where this subject stays current on the one collections socket. `subscribe` holds the extra
 * fields of the `subscribe` command (`terminal`, `incarnation`, `conversation`, filters);
 * `requires` names an action that must succeed first (`terminal.attach` mints the capability).
 */
export const LiveChannel = Schema.Struct({
  collection: Schema.Literals([
    'missions',
    'attention',
    'agents',
    'work',
    'terminal',
    'conversation',
  ]),
  key: SubjectRef,
  subscribe: Schema.Record(Schema.String, Schema.Union([Schema.String, Schema.Finite])),
  requires: Schema.optionalKey(Schema.String),
}).annotate({ identifier: 'LiveChannel' })
export type LiveChannel = typeof LiveChannel.Type

/** Who said this, and from which snapshot. */
export const Provenance = Schema.Struct({
  source: Schema.Literals(['projection', 'observer', 'runtime', 'harness']),
  snapshot_id: Schema.String,
  host_id: SubjectRef,
  observer: Schema.optionalKey(SubjectRef),
  provider: Schema.optionalKey(Schema.String),
}).annotate({ identifier: 'Provenance' })
export type Provenance = typeof Provenance.Type

/** Present when `data` is knowingly incomplete; lists omitted top-level keys and why. */
export const Partial = Schema.Struct({
  omitted: Schema.Array(Schema.String),
  reason: Schema.Literals(['windowed', 'forbidden', 'pending', 'truncated']),
}).annotate({ identifier: 'Partial' })
export type Partial = typeof Partial.Type

/** Every envelope field except `schema` and `data`; shared by all schema members. */
export const envelopeHeaderFields = {
  ref: SubjectRef,
  family: Family,
  revision: Revision,
  observed_at: Timestamp,
  provenance: Provenance,
  actions: Schema.Array(ActionOffer),
  live: Schema.NullOr(LiveChannel),
  partial: Schema.optionalKey(Partial),
} as const

/** Open fact bag: what `data` is before a renderer's schema claims it. */
export const FactBag = Schema.Record(Schema.String, Schema.Unknown).annotate({
  identifier: 'FactBag',
})
export type FactBag = typeof FactBag.Type

/** Envelope with any schema id and undecoded data — the routing view every client can read. */
export const SubjectEnvelope = Schema.Struct({
  ...envelopeHeaderFields,
  schema: SchemaId,
  data: FactBag,
}).annotate({ identifier: 'SubjectEnvelope' })
export type SubjectEnvelope = typeof SubjectEnvelope.Type

/** Builds the typed envelope member for one schema id. */
export const envelopeOf = <const Id extends string, Data extends Schema.Top>({
  id,
  data,
}: {
  readonly id: Id
  readonly data: Data
}) =>
  Schema.Struct({ ...envelopeHeaderFields, schema: Schema.Literal(id), data }).annotate({
    identifier: id,
  })

/**
 * Decodes `input` with `schema`, returning the value or the decode issue as text. Excess keys are
 * ignored (the Effect default), so newer minors decode under older schemas.
 */
export const decodeWith = <A>({
  schema,
  input,
}: {
  readonly schema: Schema.Decoder<A>
  readonly input: unknown
}): { readonly ok: true; readonly value: A } | { readonly ok: false; readonly issue: string } => {
  const result = Schema.decodeUnknownResult(schema)(input)
  return Result.isSuccess(result)
    ? { ok: true, value: result.success }
    : { ok: false, issue: result.failure.message }
}

/** A decoded routing view, or the raw input with the header decode issue. */
export type EnvelopeDecode =
  | { readonly _tag: 'Envelope'; readonly envelope: SubjectEnvelope }
  | { readonly _tag: 'Malformed'; readonly input: unknown; readonly issue: string }

/** Decodes the routing view; data stays an open bag until a renderer claims it. */
export const decodeEnvelope = (input: unknown): EnvelopeDecode => {
  const decoded = decodeWith({ schema: SubjectEnvelope, input })
  return decoded.ok
    ? { _tag: 'Envelope', envelope: decoded.value }
    : { _tag: 'Malformed', input, issue: decoded.issue }
}
