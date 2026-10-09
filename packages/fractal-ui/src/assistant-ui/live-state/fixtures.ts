import type { Activity, ActivityRecord, Axis, Fact, LiveStateSnapshot, ObservationAxis, Progress, Time } from './types'
export const diagnosticNow = Date.parse('2026-06-12T10:00:00.000Z')
export const diagnosticTime = (offset: number): Time => new Date(diagnosticNow + offset).toISOString()
export function known<T>(value: T, observedOffset = -1000, validityOffset = 60000): Fact<T> {
  return { kind: 'known', value, freshness: { state: 'current', basis: 'observation', source_index: 42, observed_at: diagnosticTime(observedOffset), valid_until: diagnosticTime(validityOffset), reason: null } }
}
function axis<T>(value: T): Axis<T> {
  return { support: { kind: 'supported', version: 1 }, fact: known(value), observation: { observed_at: diagnosticTime(-1000), received_at: diagnosticTime(-500), source_sequence: '42' } }
}
const taskProgress: Progress = { kind: 'tasks', snapshot: { harness: 'fixture-harness', session_id: 'session-iris', incarnation_id: 'incarnation-iris', observed_at: diagnosticTime(-1000), source_op: 'todo-snapshot', phases: [{ name: 'Verification', tasks: [{ content: 'Read interface contract', status: 'completed' }, { content: 'Wire keyboard disclosure', status: 'in_progress' }, { content: 'Review host receipt', status: 'blocked', blocker: 'Waiting for owner receipt' }, { content: 'Publish the result', status: 'pending' }] }], totals: { completed: 1, in_progress: 1, blocked: 1, pending: 1 }, truncated: false } }
const toolRecord: ActivityRecord = { activity: known<Activity>({ kind: 'tool_running', call_id: 'call-iris', tool: 'shell', started_at: known(diagnosticTime(-59000)) }), since: known(diagnosticTime(-59000)), scope: { kind: 'tool', call_id: 'call-iris', request_id: known('request-iris') } }
export const allKnown: LiveStateSnapshot = {
  key: known({ seat_id: 'seat-iris', owner_host_id: 'host-iris', runtime_incarnation: 'incarnation-iris', native_session_id: 'session-iris', execution_id: 'execution-iris' }), active_turn_id: known('turn-iris'),
  runtime: axis({ state: 'running', runtime_id: known('runtime-iris') }), activity: axis([toolRecord]),
  needs_you: axis([{ ask: 'permission', request_id: known('request-iris'), call_id: known('call-iris'), person_id: known('person-iris') }]), quota_retry: axis({ kind: 'retrying', attempt: 3, code: 'capacity', retry_at: known(diagnosticTime(65000)) }), progress: axis(taskProgress),
  heartbeat: axis({ kind: 'heartbeat', at: diagnosticTime(-1000) }), host_reachability: axis({ kind: 'host_reachability', state: 'online', last_success_at: known(diagnosticTime(-1000)) }), harness_exit: axis(null),
  heartbeat_policy: known({ version: 1, cadence_ms: 20000, freshness_threshold_ms: 90000, lease_horizon_ms: 120000, allowed_clock_skew_ms: 1000, on_coverage_loss: 'invalidate' }),
}
export const observationAxes: readonly ObservationAxis[] = ['runtime', 'activity', 'needs_you', 'quota_retry', 'progress', 'heartbeat', 'host_reachability', 'harness_exit']
export type DiagnosticScenario = 'all-known' | `missing-${ObservationAxis}` | 'unsupported' | 'stale-heartbeat' | 'stale-activity' | 'expired-activity' | 'offline' | 'crash' | 'needs-you' | 'quota-countdown' | 'retry-countdown' | 'long-tool' | 'missing-tool-start' | 'progress-only' | 'runtime-only' | 'heartbeat-only' | 'milestones' | 'percent' | 'concurrent' | 'truncated-tasks' | 'keyboard' | 'reduced-motion'
/** Synthetic admitted snapshots. Controls remove the exact observation the play expects. */
export function diagnosticFixture(scenario: DiagnosticScenario, control = false): LiveStateSnapshot {
  let snapshot = allKnown
  if (scenario.startsWith('missing-') && scenario !== 'missing-tool-start') {
    const name = scenario.slice(8) as ObservationAxis
    return { ...snapshot, [name]: { ...snapshot[name], fact: control ? snapshot[name].fact : { kind: 'unknown', reason: 'source_lost' } } }
  }
  if (scenario === 'unsupported') {
    return Object.assign({}, snapshot, ...observationAxes.map(name => ({ [name]: { ...snapshot[name], support: control ? snapshot[name].support : { kind: 'unsupported', reason: 'No producer in this adapter version' } } })))
  }
  if (scenario === 'stale-heartbeat') return { ...snapshot, heartbeat: axis({ kind: 'heartbeat', at: diagnosticTime(control ? -1000 : -180000) }) }
  if (scenario === 'crash') return { ...snapshot, runtime: axis({ state: 'failed', runtime_id: known('runtime-iris') }), host_reachability: axis({ kind: 'host_reachability', state: 'offline', last_success_at: known(diagnosticTime(-10000)) }), harness_exit: axis({ kind: 'harness_exit', at: diagnosticTime(-1000), exit_code: known(1), signal: known('SIGABRT'), cause: known(control ? 'stopped' : 'crashed') }) }
  if (scenario === 'offline') return { ...snapshot, host_reachability: axis({ kind: 'host_reachability', state: control ? 'online' : 'offline', last_success_at: known(diagnosticTime(-10000)) }) }
  if (scenario === 'keyboard' || scenario === 'reduced-motion') return snapshot
  if (scenario === 'needs-you' || scenario === 'all-known') return control ? { ...snapshot, needs_you: axis(null) } : snapshot
  snapshot = { ...snapshot, needs_you: axis(null) }
  if (scenario === 'quota-countdown') return { ...snapshot, quota_retry: axis({ kind: 'quota_wait', account_id: known('account-iris'), resume_at: known(diagnosticTime(65000)) }) }
  if (scenario === 'retry-countdown') return { ...snapshot, quota_retry: axis({ kind: 'retrying', attempt: 3, code: 'capacity', retry_at: known(diagnosticTime(65000)) }) }
  snapshot = { ...snapshot, quota_retry: axis({ kind: 'not_waiting' }) }
  if (scenario === 'stale-activity' || scenario === 'expired-activity') {
    const historical: ActivityRecord = { ...toolRecord, activity: known<Activity>({ kind: 'tool_running', call_id: 'call-iris', tool: 'shell', started_at: known(diagnosticTime(-240000), -180000, -120000) }, -180000, -120000), since: known(diagnosticTime(-240000), -180000, -120000) }
    const current = control ? known([toolRecord]) : known([historical], -180000, -120000)
    return { ...snapshot, activity: { ...snapshot.activity, observation: control ? snapshot.activity.observation : { observed_at: diagnosticTime(-180000), received_at: diagnosticTime(-179500), source_sequence: '42' }, fact: current.kind === 'known' ? { ...current, freshness: { ...current.freshness, state: scenario === 'stale-activity' && !control ? 'not_current' : 'current', reason: 'Observer coverage receipt' } } : current } }
  }
  if (scenario === 'long-tool' || scenario === 'missing-tool-start' || scenario === 'concurrent') {
    const start = scenario === 'missing-tool-start' && !control ? { kind: 'unknown' as const, reason: 'not_observed' as const } : known(diagnosticTime(-35999000))
    const record: ActivityRecord = { ...toolRecord, activity: known({ kind: 'tool_running', call_id: 'call-iris', tool: 'shell', started_at: start }) }
    const records = scenario === 'concurrent' ? [record, { ...toolRecord, activity: known<Activity>({ kind: 'thinking', request_id: 'request-lumen' }), scope: { kind: 'request' as const, request_id: 'request-lumen' } }] : [record]
    return { ...snapshot, activity: control && scenario === 'concurrent' ? axis([]) : axis(records) }
  }
  if (scenario === 'milestones') return { ...snapshot, progress: axis(control ? taskProgress : { kind: 'milestones', completed: 2, total: 5, unit: 'review gates' }) }
  if (scenario === 'percent') return { ...snapshot, progress: axis(control ? taskProgress : { kind: 'reported_percent', value: 35, meaning: 'producer-reported verification' }) }
  if (scenario === 'truncated-tasks' && taskProgress.kind === 'tasks') return { ...snapshot, progress: axis({ ...taskProgress, snapshot: { ...taskProgress.snapshot, truncated: !control } }) }
  if (scenario === 'progress-only' || scenario === 'runtime-only' || scenario === 'heartbeat-only') {
    const keep = scenario === 'progress-only' ? 'progress' : scenario === 'runtime-only' ? 'runtime' : 'heartbeat'
    return Object.assign({}, snapshot, ...observationAxes.filter(name => name !== keep || control).map(name => ({ [name]: { ...snapshot[name], fact: { kind: 'unknown', reason: 'not_observed' } } })))
  }
  return snapshot
}
