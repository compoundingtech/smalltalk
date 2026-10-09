/** Adapter boundary mirroring the proposed diagnostics observations, not an SDK dependency.
 * Wire/schema changes belong here. A host supplies an admitted, per-execution snapshot;
 * presentation never correlates runs, invents phase edges, or reduces transport events.
 */
export type Id = string
export type Time = string
export interface Freshness {
  readonly state: 'current' | 'not_current'
  readonly basis: 'declaration' | 'projection' | 'observation' | 'history'
  readonly source_index: number | null
  readonly observed_at: Time | null
  readonly valid_until: Time | null
  readonly reason: string | null
}
export type Fact<T> =
  | { readonly kind: 'known'; readonly value: T; readonly freshness: Freshness }
  | { readonly kind: 'unknown'; readonly reason: 'not_observed' | 'unsupported_observer' | 'source_lost' | 'lease_expired' | 'identity_unresolved' | 'read_failed' | 'payload_too_large' }
export type Support = { readonly kind: 'supported'; readonly version: number } | { readonly kind: 'unsupported'; readonly reason: string }
export type ObservationAxis = 'runtime' | 'activity' | 'needs_you' | 'quota_retry' | 'progress' | 'heartbeat' | 'host_reachability' | 'harness_exit'
export interface ExecutionKey {
  readonly seat_id: Id
  readonly owner_host_id: Id
  readonly runtime_incarnation: Id
  readonly native_session_id: Id
  readonly execution_id: Id
}
export interface Observation { readonly observed_at: Time; readonly received_at: Time; readonly source_sequence: string }
export type ObservationScope = { readonly kind: 'execution' } | { readonly kind: 'request'; readonly request_id: Id } | { readonly kind: 'tool'; readonly call_id: Id; readonly request_id: Fact<Id> }
export type Activity =
  | { readonly kind: 'ready' | 'idle' | 'active' }
  | { readonly kind: 'thinking'; readonly request_id: Id }
  | { readonly kind: 'tool_running'; readonly call_id: Id; readonly tool: string; readonly started_at: Fact<Time> }
  | { readonly kind: 'streaming'; readonly request_id: Id; readonly channel: 'text' | 'reasoning'; readonly last_chunk_at: Time }
  | { readonly kind: 'waiting_dependency'; readonly targets: readonly Id[] }
  | { readonly kind: 'compacting'; readonly started_at: Time; readonly trigger: Fact<'manual' | 'auto'> }
export interface NeedsYou { readonly request_id: Fact<Id>; readonly call_id: Fact<Id>; readonly person_id: Fact<Id>; readonly ask: 'permission' | 'question' | 'review' }
export type QuotaRetry =
  | { readonly kind: 'not_waiting' }
  | { readonly kind: 'quota_wait'; readonly account_id: Fact<Id>; readonly resume_at: Fact<Time> }
  | { readonly kind: 'retrying'; readonly attempt: number; readonly code: string; readonly retry_at: Fact<Time> }
export interface HarnessTodoSnapshot {
  readonly harness: string
  readonly incarnation_id: Id
  readonly observed_at: Time
  readonly phases: readonly { readonly name: string; readonly tasks: readonly { readonly content: string; readonly status: 'pending' | 'in_progress' | 'completed' | 'blocked'; readonly blocker?: string | null }[] }[]
  readonly session_id: Id
  readonly source_op: string
  readonly totals: { readonly abandoned?: number; readonly blocked: number; readonly completed: number; readonly in_progress: number; readonly pending: number }
  readonly truncated: boolean
}
export type Progress = { readonly kind: 'tasks'; readonly snapshot: HarnessTodoSnapshot } | { readonly kind: 'milestones'; readonly completed: number; readonly total: number; readonly unit: string } | { readonly kind: 'reported_percent'; readonly value: number; readonly meaning: string }
export interface Runtime { readonly state: 'starting' | 'running' | 'exited' | 'failed'; readonly runtime_id: Fact<Id> }
export type Heartbeat = { readonly kind: 'heartbeat'; readonly at: Time }
export type HostReachability = { readonly kind: 'host_reachability'; readonly state: 'online' | 'offline'; readonly last_success_at: Fact<Time> }
export type HarnessExit = { readonly kind: 'harness_exit'; readonly at: Time; readonly exit_code: Fact<number>; readonly signal: Fact<string>; readonly cause: Fact<'completed' | 'stopped' | 'crashed'> }
export type Health = Heartbeat | HostReachability | HarnessExit
export interface HeartbeatPolicy {
  readonly version: number
  readonly cadence_ms: number
  readonly freshness_threshold_ms: number
  readonly lease_horizon_ms: number
  readonly allowed_clock_skew_ms: number
  readonly on_coverage_loss: 'invalidate'
}
export interface AdapterSupport {
  readonly adapter: string
  readonly adapter_version: string
  readonly root_axes: Readonly<Record<ObservationAxis, Support>>
  readonly child_axes: Readonly<Record<ObservationAxis, Support>>
  readonly child_inventory: Support
  readonly child_conversation: Support
  readonly exact_turn: Support
  readonly tool_start: Support
  readonly token_deltas: Support
  readonly heartbeat_policy: Fact<HeartbeatPolicy>
}
export type ExecutionObservation = { readonly key: ExecutionKey; readonly turn_id: Fact<Id>; readonly scope: ObservationScope; readonly observation: Observation } & (
  | { readonly kind: 'runtime_observed'; readonly runtime: Fact<Runtime> }
  | { readonly kind: 'activity_changed'; readonly since: Fact<Time>; readonly activity: Fact<Activity> }
  | { readonly kind: 'needs_you_observed'; readonly needs_you: Fact<NeedsYou | null> }
  | { readonly kind: 'quota_retry_observed'; readonly quota_retry: Fact<QuotaRetry> }
  | { readonly kind: 'progress_observed'; readonly progress: Fact<Progress> }
  | { readonly kind: 'health_observed'; readonly health: Fact<Health> }
)
export interface Axis<T> { readonly support: Support; readonly fact: Fact<T>; readonly observation: Observation | null }
export interface ActivityRecord { readonly activity: Fact<Activity>; readonly since: Fact<Time>; readonly scope: ObservationScope }
/** Snapshot mapping is host-owned. Retain concurrent activities and human asks; a completion
 * in one scope must not clear another. A known empty array means positively observed none.
 */
export interface LiveStateSnapshot {
  readonly key: Fact<ExecutionKey>
  readonly active_turn_id: Fact<Id | null>
  readonly runtime: Axis<Runtime>
  readonly activity: Axis<readonly ActivityRecord[]>
  readonly needs_you: Axis<readonly NeedsYou[] | null>
  readonly quota_retry: Axis<QuotaRetry>
  readonly progress: Axis<Progress>
  readonly heartbeat: Axis<Heartbeat>
  readonly host_reachability: Axis<HostReachability>
  readonly harness_exit: Axis<HarnessExit | null>
  readonly heartbeat_policy: Fact<HeartbeatPolicy>
}
