// moves to smalltalk/clients/typescript
/**
 * Data schemas for the first envelope schema ids, plus the schema-id-indexed catalog and the
 * typed `KnownEnvelope` union.
 *
 * Shapes derive from smalltalk `clients/typescript/st3-client/Models.generated.ts` (Mission,
 * MissionRunSummary, Agent, WorkLabel, Attention, Runtime, TerminalScreen) and the fact lists in
 * `docs/st3/schema.md` (`vcs.pull-request`, `ci.run`). Header fields (`id`, `kind`, `revision`)
 * move to the envelope; `data` is the rest of the subject body.
 */
import { Schema } from 'effect'

import { envelopeOf, SubjectRef, Timestamp } from './envelope.ts'

const optional = Schema.optionalKey
const nullable = Schema.NullOr

/** Who must act next on a mission or run (client-v0 `must_act`). */
export const MustAct = Schema.Literals(['you', 'agent', 'system', 'blocked', 'nobody'])

const MissionRunSummary = Schema.Struct({
  id: SubjectRef,
  phase: Schema.String,
  status: Schema.String,
  must_act: MustAct,
  progress: Schema.Struct({ done: Schema.Finite, total: Schema.Finite }),
  requester: SubjectRef,
  state_since: Timestamp,
  last_progress: Schema.String.pipe(nullable, optional),
  deadline: Timestamp.pipe(nullable, optional),
  outcome: Schema.Struct({
    status: Schema.Literals(['completed', 'failed', 'cancelled']),
    reason: Schema.String,
    at: Timestamp,
  }).pipe(nullable, optional),
  current_steps: Schema.Array(
    Schema.Struct({
      id: SubjectRef,
      title: nullable(Schema.String),
      state: Schema.String,
      since: Timestamp,
      assignee: SubjectRef.pipe(nullable, optional),
    }),
  ),
}).annotate({ identifier: 'MissionRunSummary' })

/** `st3.mission@1` — client-v0 `Mission` body. */
export const MissionData = Schema.Struct({
  title: Schema.String,
  state: Schema.Literals([
    'draft',
    'ready',
    'retired',
    'running',
    'standing',
    'completed',
    'failed',
    'cancelled',
  ]),
  mission_revision: Schema.String,
  must_act: optional(MustAct),
  active_runs: optional(Schema.Finite),
  runs: Schema.Array(SubjectRef),
  run_details: optional(Schema.Array(MissionRunSummary)),
  updated_at: Timestamp,
}).annotate({ identifier: 'MissionData' })
export type MissionData = typeof MissionData.Type

/** client-v0 `WorkLabel`: one step in an agent's queue. */
export const WorkLabel = Schema.Struct({
  id: SubjectRef,
  mission_id: SubjectRef,
  mission_run_id: SubjectRef,
  path: Schema.String,
  since: Timestamp,
  state: Schema.String,
  title: Schema.String.pipe(nullable, optional),
  goal: Schema.String.pipe(nullable, optional),
}).annotate({ identifier: 'WorkLabel' })

/** `st3.agent@1` — client-v0 `Agent` body (queue, delivery and usage trimmed). */
export const AgentData = Schema.Struct({
  name: Schema.String,
  state: Schema.Literals(['desired', 'starting', 'running', 'waiting', 'stopped', 'failed']),
  reachability: Schema.Literals(['local', 'remote', 'unreachable', 'unknown']),
  driver: Schema.String.pipe(nullable, optional),
  harness_state: Schema.String.pipe(nullable, optional),
  fault: Schema.String.pipe(nullable, optional),
  current_work: optional(Schema.Array(WorkLabel)),
  next_work: WorkLabel.pipe(nullable, optional),
  queued_work_count: optional(Schema.Finite),
  last_activity_at: Timestamp.pipe(nullable, optional),
  silent_since: Timestamp.pipe(nullable, optional),
}).annotate({ identifier: 'AgentData' })
export type AgentData = typeof AgentData.Type

/**
 * `st3.pty@1` — a mission-run terminal runtime: client-v0 `Runtime` (runtime_kind `terminal`)
 * joined with a text preview of its `TerminalScreen`. The full screen streams over `live`.
 */
export const PtyData = Schema.Struct({
  runtime_id: Schema.String,
  state: Schema.Literals([
    'pending',
    'starting',
    'running',
    'stopping',
    'stopped',
    'exited',
    'failed',
    'unreachable',
  ]),
  owner_id: SubjectRef,
  terminal_id: nullable(SubjectRef),
  incarnation_id: nullable(Schema.String),
  title: Schema.String,
  columns: Schema.Finite,
  rows: Schema.Finite,
  /** Last visible screen lines as plain text (`TerminalLine.text`), oldest first. */
  preview: Schema.Array(Schema.String),
  exit_code: Schema.Finite.pipe(nullable, optional),
}).annotate({ identifier: 'PtyData' })
export type PtyData = typeof PtyData.Type

/** `st3.attention@1` — client-v0 `Attention` body. */
export const AttentionData = Schema.Struct({
  attention_kind: Schema.Literals([
    'human-gate',
    'launch-approval',
    'revision-approval',
    'unread-message',
    'agent-request',
    'fault',
  ]),
  title: Schema.String,
  detail: Schema.String,
  priority: Schema.Literals(['critical', 'high', 'normal', 'low']),
  state: Schema.Literals(['open', 'resolved']),
  requested_at: Timestamp,
  person_id: SubjectRef,
  source_id: SubjectRef,
  because: optional(Schema.String),
  what: optional(Schema.String),
  mission_id: optional(SubjectRef),
}).annotate({ identifier: 'AttentionData' })
export type AttentionData = typeof AttentionData.Type

/**
 * `checks`/`reviews` are `array` facts with unspecified items in schema.md; these item shapes
 * are an [INFERENCE] from the github-pr provider and stay open (extra keys kept).
 */
const openItem = <F extends Schema.Struct.Fields>(fields: F) =>
  Schema.StructWithRest(Schema.Struct(fields), [Schema.Record(Schema.String, Schema.Unknown)])

/** `st3.resource:vcs.pull-request@1` — `vcs.pull-request` facts (schema.md). */
export const PullRequestData = Schema.Struct({
  number: Schema.Int,
  title: Schema.String,
  state: Schema.String,
  url: Schema.String,
  repository: SubjectRef,
  author: optional(Schema.String),
  branch: optional(Schema.String),
  base: optional(SubjectRef),
  head: optional(SubjectRef),
  head_sha: optional(Schema.String),
  draft: optional(Schema.Boolean),
  merged: optional(Schema.Boolean),
  created_at: optional(Timestamp),
  updated_at: optional(Timestamp),
  opened_by: optional(SubjectRef),
  opened_by_run: optional(SubjectRef),
  checks: optional(
    Schema.Array(
      openItem({ name: Schema.String, status: Schema.String, conclusion: nullable(Schema.String) }),
    ),
  ),
  reviews: optional(Schema.Array(openItem({ reviewer: Schema.String, state: Schema.String }))),
}).annotate({ identifier: 'PullRequestData' })
export type PullRequestData = typeof PullRequestData.Type

/** `st3.resource:ci.run@1` — `ci.run` facts (schema.md). */
export const CiRunData = Schema.Struct({
  name: Schema.String,
  status: Schema.String,
  provider: Schema.String,
  url: Schema.String,
  repository: SubjectRef,
  external_id: optional(Schema.String),
  conclusion: optional(Schema.String),
  commit: optional(SubjectRef),
  pull_request: optional(SubjectRef),
  started_at: optional(Timestamp),
  completed_at: optional(Timestamp),
}).annotate({ identifier: 'CiRunData' })
export type CiRunData = typeof CiRunData.Type

/** Schema id → data schema. The SDK's single source for "which schemas do we know". */
export const dataSchemas = {
  'st3.mission@1': MissionData,
  'st3.agent@1': AgentData,
  'st3.pty@1': PtyData,
  'st3.attention@1': AttentionData,
  'st3.resource:vcs.pull-request@1': PullRequestData,
  'st3.resource:ci.run@1': CiRunData,
} as const

/** A schema id this client has a data schema for. */
export type KnownSchemaId = keyof typeof dataSchemas
/** Decoded `data` type for a known schema id. */
export type DataOf<Id extends KnownSchemaId> = (typeof dataSchemas)[Id]['Type']

const MissionEnvelope = envelopeOf({ id: 'st3.mission@1', data: MissionData })
const AgentEnvelope = envelopeOf({ id: 'st3.agent@1', data: AgentData })
const PtyEnvelope = envelopeOf({ id: 'st3.pty@1', data: PtyData })
const AttentionEnvelope = envelopeOf({ id: 'st3.attention@1', data: AttentionData })
const PullRequestEnvelope = envelopeOf({
  id: 'st3.resource:vcs.pull-request@1',
  data: PullRequestData,
})
const CiRunEnvelope = envelopeOf({ id: 'st3.resource:ci.run@1', data: CiRunData })

/** Typed union of every known envelope, discriminated by `schema`. */
export const KnownEnvelope = Schema.Union(
  [
    MissionEnvelope,
    AgentEnvelope,
    PtyEnvelope,
    AttentionEnvelope,
    PullRequestEnvelope,
    CiRunEnvelope,
  ],
  { mode: 'oneOf' },
).annotate({ identifier: 'KnownEnvelope' })
export type KnownEnvelope = typeof KnownEnvelope.Type
/** The typed envelope for one known schema id. */
export type EnvelopeOf<Id extends KnownSchemaId> = Extract<KnownEnvelope, { readonly schema: Id }>
