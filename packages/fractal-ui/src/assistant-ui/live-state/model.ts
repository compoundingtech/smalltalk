import type { AgentStatus } from '../sidebar/model'
import type { ActivityRecord, Axis, Fact, LiveStateSnapshot, ObservationAxis, Time } from './types'

export interface DiagnosticLine {
  readonly axis: ObservationAxis
  readonly label: string
  readonly text: string
  readonly detail: readonly string[]
  readonly stale: boolean
  readonly reported: boolean
  readonly priority: number
  readonly glyph: AgentStatus
  readonly timer?: { readonly kind: 'elapsed' | 'countdown'; readonly text: string; readonly label: string }
}
export interface LiveStateModel { readonly primary: DiagnosticLine | undefined; readonly lines: readonly DiagnosticLine[]; readonly identity: readonly string[] }
const labels: Record<ObservationAxis, string> = { runtime: 'Runtime', activity: 'Activity', needs_you: 'Needs you', quota_retry: 'Quota / retry', progress: 'Todo progress', heartbeat: 'Heartbeat', host_reachability: 'Host reachability', harness_exit: 'Harness exit' }
const reasons: Record<Extract<Fact<never>, { kind: 'unknown' }>['reason'], string> = { not_observed: 'no observation has been supplied', unsupported_observer: 'the observer does not report it', source_lost: 'the observation source was lost', lease_expired: 'the observation lease expired', identity_unresolved: 'execution identity is unresolved', read_failed: 'the observation could not be read', payload_too_large: 'the observation exceeded its size limit' }
export function duration(ms: number): string {
  const seconds = Math.max(0, Math.floor(ms / 1000))
  const hours = Math.floor(seconds / 3600)
  const minutes = Math.floor(seconds % 3600 / 60)
  return hours > 0 ? `${hours}h ${String(minutes).padStart(2, '0')}m ${String(seconds % 60).padStart(2, '0')}s` : `${minutes}m ${String(seconds % 60).padStart(2, '0')}s`
}
function stale<T>(fact: Fact<T>, now: number): boolean {
  return fact.kind === 'known' && (fact.freshness.state === 'not_current' || (fact.freshness.valid_until !== null && now >= Date.parse(fact.freshness.valid_until)))
}
function field<T>(label: string, fact: Fact<T>, now: number): string {
  if (fact.kind === 'unknown') return `${label} isn't reported: ${reasons[fact.reason]}.`
  return `${label}: ${fact.value === null ? 'none reported' : String(fact.value)}${stale(fact, now) ? ' (stale)' : ''}`
}
function line<T>(axis: ObservationAxis, source: Axis<T>, now: number, make: (value: T, at: number, current: boolean) => Partial<DiagnosticLine> & { text: string }): DiagnosticLine {
  const base = { axis, label: labels[axis], priority: 99, glyph: 'unobserved' as const, detail: [], stale: false, reported: false }
  if (source.support.kind === 'unsupported') return { ...base, text: `${labels[axis]} isn't reported by this adapter: ${source.support.reason}.` }
  const fact = source.fact
  if (fact.kind === 'unknown') return { ...base, text: `${labels[axis]} isn't reported: ${reasons[fact.reason]}.` }
  const isStale = stale(fact, now)
  const observedAt = fact.freshness.observed_at ?? source.observation?.observed_at
  // Historical timers never tick; preserve the producer's age without inventing a timestamp.
  const value = make(fact.value, isStale && observedAt !== undefined ? Date.parse(observedAt) : now, !isStale)
  return { ...base, ...value, reported: true, stale: isStale || value.stale === true, ...(isStale ? { glyph: 'stale' as const, text: `${value.text} · stale${observedAt === undefined ? "; observation age isn't reported" : ` ${duration(now - Date.parse(observedAt))} ago`}`, timer: undefined } : {}), detail: [...(value.detail ?? []), `Adapter support version ${source.support.version}.`, source.observation === null ? "Observation provenance isn't reported." : `Observed ${source.observation.observed_at}; admitted ${source.observation.received_at}; sequence ${source.observation.source_sequence}.`, `Freshness basis: ${fact.freshness.basis}${fact.freshness.source_index === null ? "; source index isn't reported" : `; source index ${fact.freshness.source_index}`}${fact.freshness.valid_until === null ? '; validity deadline is not reported' : `; valid until ${fact.freshness.valid_until}`}.${fact.freshness.reason === null ? '' : ` ${fact.freshness.reason}`}`] }
}
function activity(record: ActivityRecord, now: number, current: boolean): { text: string; detail: string[]; stale: boolean; reported: boolean; glyph: AgentStatus; timer?: DiagnosticLine['timer'] } {
  const detail = [record.scope.kind === 'execution' ? 'Execution scope' : record.scope.kind === 'request' ? `Request: ${record.scope.request_id}` : `Tool call: ${record.scope.call_id}; ${field('Request', record.scope.request_id, now)}`]
  if (record.activity.kind === 'unknown') return { text: `Activity isn't reported: ${reasons[record.activity.reason]}.`, detail, stale: false, reported: false, glyph: 'unobserved' }
  const value = record.activity.value
  const phase = value.kind === 'tool_running' ? `Tool: ${value.tool}` : value.kind === 'waiting_dependency' ? 'Waiting for dependency' : value.kind === 'streaming' ? `Streaming ${value.channel}` : value.kind.charAt(0).toUpperCase() + value.kind.slice(1)
  const start = value.kind === 'tool_running' ? value.started_at : record.since
  const isStale = stale(record.activity, now)
  const timer = current && !isStale && start.kind === 'known' && !stale(start, now) ? { kind: 'elapsed' as const, text: duration(now - Date.parse(start.value)), label: `${phase} elapsed` } : undefined
  if (value.kind === 'tool_running') detail.push(`Call: ${value.call_id}`, field('Tool start', value.started_at, now))
  if (value.kind === 'thinking' || value.kind === 'streaming') detail.push(`Request: ${value.request_id}`)
  if (value.kind === 'streaming') detail.push(`Last chunk: ${value.last_chunk_at}`)
  if (value.kind === 'waiting_dependency') detail.push(`Dependencies: ${value.targets.join(', ')}`)
  if (value.kind === 'compacting') detail.push(`Compaction start: ${value.started_at}`, field('Trigger', value.trigger, now))
  detail.push(field('Phase start', record.since, now))
  const observedAt = record.activity.freshness.observed_at
  return { text: isStale ? `${phase} · stale${observedAt === null ? "; observation age isn't reported" : ` ${duration(now - Date.parse(observedAt))} ago`}` : phase, detail, timer, stale: isStale, reported: true, glyph: isStale ? 'stale' : ['active', 'thinking', 'tool_running', 'streaming', 'compacting'].includes(value.kind) ? 'working' : 'idle' }
}
/** Compact order: positive crash > owner-host offline > human ask > quota/retry >
 * activity > explicit progress > runtime > heartbeat. Historical facts never outrank
 * current observations; every retained axis remains in the header and disclosure.
 */
export function liveStateModel(snapshot: LiveStateSnapshot, now: number): LiveStateModel {
  const lines: DiagnosticLine[] = [
    line('runtime', snapshot.runtime, now, value => ({ text: `Runtime ${value.state}`, priority: 6, glyph: value.state === 'running' ? 'working' : value.state === 'starting' ? 'pending' : 'ended', detail: [field('Runtime ID', value.runtime_id, now)] })),
    line('activity', snapshot.activity, now, (values, at, current) => {
      const activities = values.map(value => activity(value, at, current))
      const selected = [...activities].sort((a, b) => Number(b.reported) - Number(a.reported) || Number(a.stale) - Number(b.stale) || Number(b.glyph === 'working') - Number(a.glyph === 'working'))[0]
      return { text: selected?.text ?? 'No active activity reported', priority: selected !== undefined && !selected.reported ? 99 : 4, glyph: selected?.glyph ?? 'idle', stale: selected?.stale, timer: selected?.timer, detail: activities.flatMap(value => [value.text + (value.timer ? ` · ${value.timer.text}` : ''), ...value.detail]) }
    }),
    line('needs_you', snapshot.needs_you, now, values => ({ text: values === null || values.length === 0 ? 'No human ask reported' : `Needs you: ${values[0].ask}${values.length > 1 ? ` +${values.length - 1}` : ''}`, priority: values !== null && values.length > 0 ? 2 : 99, glyph: values !== null && values.length > 0 ? 'waiting' : 'idle', detail: values?.flatMap(value => [`Ask: ${value.ask}`, field('Request', value.request_id, now), field('Call', value.call_id, now), field('Person', value.person_id, now)]) ?? [] })),
    line('quota_retry', snapshot.quota_retry, now, (value, at) => {
      if (value.kind === 'not_waiting') return { text: 'No quota or retry wait reported', priority: 99, glyph: 'idle' }
      const deadline = value.kind === 'quota_wait' ? value.resume_at : value.retry_at
      const timer = deadline.kind === 'known' && !stale(deadline, at) ? { kind: 'countdown' as const, text: Date.parse(deadline.value) > at ? duration(Date.parse(deadline.value) - at) : 'due', label: value.kind === 'quota_wait' ? 'Quota resume countdown' : 'Retry countdown' } : undefined
      return { text: value.kind === 'quota_wait' ? 'Quota wait' : `Retry attempt ${value.attempt}`, priority: 3, glyph: 'pending', timer, detail: value.kind === 'quota_wait' ? [field('Account', value.account_id, now), field('Resume time', value.resume_at, now)] : [`Code: ${value.code}`, field('Retry time', value.retry_at, now)] }
    }),
    line('progress', snapshot.progress, now, value => {
      if (value.kind === 'milestones') return { text: `${value.completed}/${value.total} ${value.unit}`, priority: 5, detail: ['Explicit producer-reported milestones; not overall completion.'] }
      if (value.kind === 'reported_percent') return { text: `${value.value}% · ${value.meaning}`, priority: 5, detail: ['Explicit producer-reported percentage.'] }
      const todo = value.snapshot, totals = todo.totals
      // Abandoned tasks are counted separately by the producer and excluded from active progress.
      const total = totals.completed + totals.in_progress + totals.pending + totals.blocked
      return { text: `${totals.completed}/${total} tasks complete${totals.blocked > 0 ? ` · ${totals.blocked} blocked` : ''}${todo.truncated ? ' · truncated' : ''}`, priority: 5, detail: [`${totals.in_progress} in progress; ${totals.pending} pending; ${totals.blocked} blocked; ${totals.abandoned ?? 0} abandoned.`, ...todo.phases.flatMap(phase => [phase.name, ...phase.tasks.map(task => `${task.content}: ${task.status.replaceAll('_', ' ')}${task.blocker ? `; blocker: ${task.blocker}` : ''}`)]), `Harness ${todo.harness}; session ${todo.session_id}; incarnation ${todo.incarnation_id}; source ${todo.source_op}; observed ${todo.observed_at}.`, todo.truncated ? 'Task inventory is truncated; counts do not establish a complete inventory.' : 'Task counts describe tasks, not overall completion or an ETA.'] }
    }),
    line('heartbeat', snapshot.heartbeat, now, value => {
      const elapsed = now - Date.parse(value.at)
      const policy = snapshot.heartbeat_policy
      const expired = policy.kind === 'known' && !stale(policy, now) && elapsed > Math.min(policy.value.freshness_threshold_ms, policy.value.lease_horizon_ms) + policy.value.allowed_clock_skew_ms
      return { text: `${expired ? 'Stale heartbeat' : 'Heartbeat'} · ${duration(elapsed)} ago`, stale: expired, priority: 7, glyph: expired ? 'stale' : 'idle', detail: policy.kind === 'unknown' || stale(policy, now) ? ["Heartbeat freshness policy isn't reported; heartbeat age does not certify liveness."] : [`Policy v${policy.value.version}: cadence ${policy.value.cadence_ms}ms; freshness ${policy.value.freshness_threshold_ms}ms; lease ${policy.value.lease_horizon_ms}ms; allowed skew ${policy.value.allowed_clock_skew_ms}ms; invalidate on coverage loss.`] }
    }),
    line('host_reachability', snapshot.host_reachability, now, value => ({ text: `Host ${value.state}`, priority: value.state === 'offline' ? 1 : 99, glyph: value.state === 'offline' ? 'offline' : 'idle', detail: [field('Last successful host contact', value.last_success_at, now), 'Host reachability comes from owner/relay evidence, not the browser socket.'] })),
    line('harness_exit', snapshot.harness_exit, now, value => value === null ? { text: 'No harness exit reported', priority: 99, glyph: 'idle' } : ({ text: value.cause.kind === 'known' ? stale(value.cause, now) ? `Harness exited; last reported cause ${value.cause.value} (stale)` : value.cause.value === 'crashed' ? 'Harness crashed' : `Harness ${value.cause.value}` : 'Harness exited; cause is not reported', priority: value.cause.kind === 'known' && !stale(value.cause, now) && value.cause.value === 'crashed' ? 0 : 99, glyph: 'ended', detail: [`Exit at ${value.at}`, field('Exit code', value.exit_code, now), field('Signal', value.signal, now), field('Cause', value.cause, now)] })),
  ]
  const candidates = lines.filter(value => value.reported)
  const primary = [...candidates].sort((a, b) => Number(a.stale) - Number(b.stale) || a.priority - b.priority)[0]
  const key = snapshot.key
  const identity = key.kind === 'known' ? [`Seat: ${key.value.seat_id}`, `Owner host: ${key.value.owner_host_id}`, `Execution: ${key.value.execution_id}`, `Runtime incarnation: ${key.value.runtime_incarnation}`, `Native session: ${key.value.native_session_id}`, ...(stale(key, now) ? ['Execution identity is stale.'] : []), field('Active turn', snapshot.active_turn_id, now)] : [field('Execution identity', key, now)]
  return { primary, lines, identity }
}
