import type { Attention, Capabilities, Machine, Message, Mission, Work, WorkState } from '@smalltalk/st3-client'

import type { CastAgent, CastHost, CastMission, CastPerson } from './cast.ts'
import type { FactoryContext } from './context.ts'
import { scenarioId } from './context.ts'

/** Resource builders the slices share; values are client-v0 wire values. */

export const machine = (ctx: FactoryContext, host: CastHost, runtimeIds: string[], work: string[]): Machine => ({
  id: host.machine,
  kind: 'machine',
  revision: `mh-${host.name}`,
  updated_at: ctx.t.at(-30_000),
  host_id: host.id,
  name: host.name,
  state: ctx.cast.hosts[0] === host ? 'local' : 'reachable',
  fleet_id: `fleet/example-${ctx.cast.project}`,
  capacity: { state: 'unknown', reason: 'no capacity observation' },
  occupancy: { running_runtimes: runtimeIds.length },
  projects: [ctx.cast.repository],
  work,
  transports: [{ protocol: ctx.cast.hosts[0] === host ? 'unix' : 'fabric', status: ctx.cast.hosts[0] === host ? 'local' : 'up', last_success_at: ctx.t.at(-30_000) }],
  runtime_ids: runtimeIds,
})

export const mission = (ctx: FactoryContext, value: CastMission, state: Mission['state'], updatedMs: number): Mission => ({
  id: value.id,
  kind: 'mission',
  revision: `mr-${value.steps.length}`,
  updated_at: ctx.t.at(updatedMs),
  title: value.title,
  state,
  mission_revision: `sha256:${ctx.cast.project}-mission-1`,
  runs: [value.run],
  run_generations: { [value.run]: value.generation },
  active_runs: state === 'running' ? 1 : 0,
  total_runs: 1,
})

export interface WorkInput {
  readonly mission: CastMission
  readonly step: number
  readonly state: WorkState
  readonly updatedMs: number
  readonly claimant?: CastAgent
  readonly blockedReason?: string
  readonly goals: string[]
}

export const work = (ctx: FactoryContext, input: WorkInput): Work => {
  const step = input.mission.steps[input.step]!
  return {
    id: step.work,
    kind: 'work',
    revision: `w-${input.step + 1}`,
    updated_at: ctx.t.at(input.updatedMs),
    mission_id: input.mission.id,
    mission_run_id: input.mission.run,
    generation_id: input.mission.generation,
    definition_id: `def-${step.path}`,
    path: step.path,
    title: step.title,
    state: input.state,
    attempt: 1,
    readiness_epoch: input.step + 1,
    claimant: input.claimant?.id ?? null,
    claim_incarnation: input.claimant?.incarnation ?? null,
    blocked_reason: input.blockedReason ?? null,
    blockers: [],
    goals: input.goals,
    constraints: ['Keep the public API stable until the docs land'],
  }
}

export interface AttentionInput {
  readonly key: string
  readonly kind: 'agent-request' | 'unread-message' | 'human-gate' | 'fault'
  readonly person: CastPerson
  readonly source: string
  readonly title: string
  readonly detail: string
  readonly priority: Attention['priority']
  readonly requestedMs: number
  readonly actions: Attention['actions']
  readonly mission?: CastMission
  readonly requester?: CastAgent
}

export const attention = (ctx: FactoryContext, input: AttentionInput): Attention => ({
  id: scenarioId(ctx, 'attention', input.key),
  kind: 'attention',
  revision: 'a1',
  updated_at: ctx.t.at(input.requestedMs),
  attention_kind: input.kind,
  source_id: input.source,
  person_id: input.person.id,
  ...(input.requester === undefined ? {} : { requester_id: input.requester.id }),
  ...(input.mission === undefined ? {} : { mission_id: input.mission.id, mission_run_id: input.mission.run }),
  title: input.title,
  detail: input.detail,
  priority: input.priority,
  state: 'open',
  requested_at: ctx.t.at(input.requestedMs),
  targets: [input.source],
  actions: input.actions,
})

export interface MessageInput {
  readonly key: string
  readonly from: string
  readonly to: string
  readonly title: string
  readonly content: string
  readonly sentMs: number
  readonly state?: Message['state']
}

export const message = (ctx: FactoryContext, input: MessageInput): Message => ({
  id: scenarioId(ctx, 'message', input.key),
  kind: 'message',
  revision: 'm1',
  updated_at: ctx.t.at(input.sentMs),
  from: input.from,
  to: input.to,
  title: input.title,
  content: input.content,
  state: input.state ?? 'delivered',
  sent_at: ctx.t.at(input.sentMs),
  in_reply_to: null,
  tags: [],
})

/** Capabilities value of a healthy daemon; `collections` v1 raises the subscription cap to 16. */
export const capabilities = (ctx: FactoryContext, options: { readonly collectionsV1?: boolean; readonly omit?: readonly string[] } = {}): Capabilities => {
  const rows = [
    { id: 'read.projections', version: 0, state: 'granted' as const },
    { id: 'terminal.read', version: 0, state: 'granted' as const },
    { id: 'terminal.attach', version: 0, state: 'granted' as const },
    { id: 'control.messages', version: 0, state: 'granted' as const },
    ...(options.collectionsV1 === false ? [] : [{ id: 'collections', version: 1, state: 'granted' as const }]),
  ]
  return {
    kind: 'capabilities',
    session_actor: `${ctx.cast.people[0]?.id ?? 'person/ada'}/session/device-1`,
    transport: 'fabric-loopback',
    capabilities: rows.filter((row) => !(options.omit ?? []).includes(row.id)),
    limits: { max_page_items: 200, max_event_items: 500, max_response_bytes: 8_388_608, max_wait_ms: 30_000 },
    event_cursor: `event-cursor/scenario-${ctx.world}/1`,
    oldest_event_cursor: `event-cursor/scenario-${ctx.world}/0`,
    schemas: ['../schemas/client-v0.schema.json', '../schemas/operations.json'],
  }
}
