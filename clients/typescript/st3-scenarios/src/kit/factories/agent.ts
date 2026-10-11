import type { Agent, Runtime, WorkLabel, WorkState } from '@smalltalk/st3-client'

import type { CastAgent, CastMission } from '../cast.ts'
import type { FactoryContext } from '../context.ts'
import { int } from '../rng.ts'

export interface AgentOptions {
  readonly state: 'desired' | 'running' | 'waiting' | 'stopped' | 'failed' | 'starting' | 'suspended'
  /** Offset of the last state change. */
  readonly sinceMs: number
  readonly lastActivityMs?: number
  readonly harnessState?: 'ready' | 'busy' | 'blocked' | 'idle'
  readonly blockedOn?: string
  readonly ask?: string
  readonly fault?: string
  readonly mission?: CastMission
  /** Index into the mission's steps for the current work. */
  readonly step?: number
  readonly workState?: WorkState
  /** Offset at which the current work entered `workState`; defaults to `sinceMs`. */
  readonly workSinceMs?: number
  readonly upcoming?: readonly number[]
  readonly reachability?: Agent['reachability']
}

export interface AgentFactoryResult {
  readonly agent: Agent
  /** Present while a runtime exists for the agent. */
  readonly runtime: Runtime | null
}

const revision = (ctx: FactoryContext, prefix: string) => `${prefix}${int(ctx.rng, 1, 40)}`

/** An `Agent` resource with joined work labels and its `Runtime` when it runs. */
export const agent = (ctx: FactoryContext, member: CastAgent, options: AgentOptions): AgentFactoryResult => {
  const { t } = ctx
  const updatedAt = t.at(options.lastActivityMs ?? options.sinceMs)
  const mission = options.mission
  const label = (index: number, state: WorkState, sinceMs: number): WorkLabel | null => {
    const step = mission?.steps[index]
    if (mission === undefined || step === undefined) return null
    return { id: step.stepRun, mission_id: mission.id, mission_run_id: mission.run, path: step.path, since: t.at(sinceMs), state, title: step.title }
  }
  const current = options.step === undefined ? null : label(options.step, options.workState ?? 'claimed', options.workSinceMs ?? options.sinceMs)
  const upcoming = (options.upcoming ?? []).flatMap((index) => label(index, 'ready', options.sinceMs) ?? [])
  const hasRuntime = options.state !== 'stopped' && options.state !== 'desired'
  const value: Agent = {
    id: member.id,
    kind: 'agent',
    revision: revision(ctx, 'ag'),
    updated_at: updatedAt,
    name: member.name,
    state: options.state,
    reachability: options.reachability ?? 'local',
    runtime_ids: hasRuntime ? [member.runtime] : [],
    owner_run_id: mission?.run ?? null,
    driver: member.driver,
    harness_state: options.harnessState ?? (options.state === 'running' ? 'busy' : 'idle'),
    since: t.at(options.sinceMs),
    host_id: member.host.id,
    last_activity_at: updatedAt,
    blocked_on: options.blockedOn ?? null,
    ask: options.ask ?? null,
    fault: options.fault ?? null,
    incarnation_id: hasRuntime ? member.incarnation : null,
    current_session_id: member.session,
    current_work_ids: current === null ? [] : [current.id],
    current_work: current === null ? [] : [current],
    active_work_count: current === null ? 0 : 1,
    next_work_id: upcoming[0]?.id ?? null,
    next_work: upcoming[0] ?? null,
    upcoming_work_ids: upcoming.map((work) => work.id),
    upcoming_work: upcoming,
    queued_work_count: upcoming.length,
    under: [],
    subagents: [],
    workspace: member.workspace,
    checkout: { repository: ctx.cast.repository, base: 'origin/main', branch: member.branch },
  }
  const runtime: Runtime | null = hasRuntime
    ? {
        id: member.runtime,
        kind: 'runtime',
        revision: revision(ctx, 'rt'),
        updated_at: t.at(options.sinceMs),
        runtime_kind: 'agent',
        owner_id: member.id,
        owner_host_id: member.host.id,
        state: options.state === 'failed' ? 'failed' : options.state === 'starting' ? 'starting' : 'running',
        runtime_id: member.runtimeId,
        incarnation_id: member.incarnation,
        desired_revision: 'desired-1',
        owner_run_id: mission?.run ?? null,
      }
    : null
  return { agent: value, runtime }
}
