/**
 * Everything wf derives from the gateway's rows, computed once for every source.
 *
 * Sources supply generated `St3.Agent` / `St3.Mission` / `St3.Attention` rows only; the fleet,
 * mission views and workbench subjects are projected here, so fixtures and live cannot derive
 * them differently. Open attention rows are the one inbox authority: a subject needs the user
 * exactly when an open attention row names it (`source_id`, `requester_id` or `mission_id`).
 */
import * as St3 from '@smalltalk/st3-client/schema'
import * as DateTime from 'effect/DateTime'
import * as Option from 'effect/Option'
import * as Schema from 'effect/Schema'

import type { MissionView, ProposedMissionFields } from '../missions/model.ts'
import type { SubjectSummary } from '../shell/context.tsx'
import type { Agent, Fleet, Host, Known } from './source.ts'

const isAgentId = Schema.is(St3.AgentId)

/** Match the native agent identity boundary before deriving any terminal subject. */
export function terminalSubjectForAgent(id: St3.AgentId): string
export function terminalSubjectForAgent(id: string): string | undefined
export function terminalSubjectForAgent(id: string): string | undefined {
  return isAgentId(id) ? `terminal/${id.slice('agent/'.length)}` : undefined
}

const harnesses = ['omp', 'claude', 'codex'] as const

/** `host/<id>` → `<id>`; an agent without a host row runs on the gateway's own host. */
const hostName = (agent: St3.Agent) =>
  Option.match(agent.host_id, { onNone: () => 'local', onSome: (id) => id.slice('host/'.length) })

/** The daemon can reach the agent's runtime now. */
const isConnected = (agent: St3.Agent) =>
  agent.reachability === 'local' ||
  agent.reachability === 'remote' ||
  agent.reachability === 'reachable'

const activityOf = (agent: St3.Agent): Agent['activity'] => {
  if (agent.state === 'failed' || Option.isSome(agent.fault)) return 'errored'
  if (agent.state === 'waiting' || Option.getOrUndefined(agent.blocked_on) === 'human')
    return 'waiting'
  const harnessState = Option.getOrUndefined(agent.harness_state)
  if (harnessState === 'working' || harnessState === 'waiting' || harnessState === 'idle')
    return harnessState
  if (agent.state === 'running' || agent.state === 'starting') return 'working'
  return 'idle'
}

const statusOf = (agent: St3.Agent): string => {
  const work = agent.current_work?.[0]
  const title = work === undefined ? undefined : Option.getOrUndefined(work.title)
  return (
    Option.getOrUndefined(agent.fault) ??
    (Option.getOrUndefined(agent.blocked_on) === 'human'
      ? `waiting for you${Option.match(agent.ask, { onNone: () => '', onSome: (ask) => ` (${ask})` })}`
      : undefined) ??
    title ??
    Option.getOrUndefined(agent.reason) ??
    Option.getOrUndefined(agent.harness_state) ??
    (typeof agent.state === 'string' ? agent.state : agent.state.raw)
  )
}

const unknown = { _tag: 'Unknown' } as const
const fromOption = <T>(value: Option.Option<T>): Known<T> =>
  Option.match(value, { onNone: () => unknown, onSome: (value) => ({ _tag: 'Known', value }) })

const lifecycleOf = (row: St3.Agent): Agent['lifecycle'] => {
  switch (row.lifecycle) {
    case 'standing': return { _tag: 'Standing' }
    case 'owner': return { _tag: 'Owner' }
    case 'bounded': return { _tag: 'Bounded' }
    case undefined: return unknown
    default: return unknown
  }
}

const agentFromRow = (row: St3.Agent): Agent => {
  const driver = Option.getOrUndefined(row.driver)
  const harness = harnesses.find((candidate) => candidate === driver)
  const mission = row.current_work?.[0]?.mission_id
  const work = row.current_work?.[0]
  const description = work === undefined ? undefined : Option.getOrUndefined(work.goal)
  const suspension = Option.getOrUndefined(row.suspension)
  const suspendedAt =
    row.state === 'suspended' && suspension !== undefined
      ? Option.getOrUndefined(suspension.suspended_at)
      : undefined
  const lastActivityAt = Option.getOrUndefined(row.last_activity_at)
  return {
    ref: row.id,
    terminal: terminalSubjectForAgent(row.id),
    ...(row.runtime_ids.length === 0 ? { terminalDisabledReason: 'This agent has no terminal.' } : {}),
    name: row.name,
    lifecycle: lifecycleOf(row),
    host: hostName(row),
    ...(harness === undefined ? {} : { harness }),
    activity: activityOf(row),
    status: statusOf(row),
    state: typeof row.state === 'string' ? row.state : row.state.raw,
    ...(description === undefined ? {} : { description }),
    usage: fromOption(row.usage),
    checkout: fromOption(row.checkout),
    workspace: fromOption(row.workspace),
    startedAt: unknown,
    endedAt: unknown,
    lastActivityAt: lastActivityAt === undefined ? unknown : { _tag: 'Known', value: DateTime.toEpochMillis(lastActivityAt) },
    blockedOn: fromOption(row.blocked_on),
    ask: fromOption(row.ask),
    ...(suspendedAt === undefined ? {} : { statusSince: DateTime.toEpochMillis(suspendedAt) }),
    ...(mission === undefined ? {} : { mission }),
    connected: isConnected(row),
  }
}

/** Hosts from `host_id`, `connected` from `reachability`, activity words from `state`/`blocked_on`. */
export const fleetFromAgents = (rows: readonly St3.Agent[]): Fleet => {
  const agents = rows.map(agentFromRow)
  const hosts = new Map<string, Host>()
  for (const agent of agents) {
    hosts.set(agent.host, {
      id: agent.host,
      connected: agent.connected || hosts.get(agent.host)?.connected === true,
    })
  }
  return { hosts: [...hosts.values()], agents }
}

/** One view per mission with its open attention; `proposed` is the fixture-only gap overlay. */
export const missionViews = ({
  missions,
  attention,
  proposed,
}: {
  readonly missions: readonly St3.Mission[]
  readonly attention: readonly St3.Attention[]
  readonly proposed?: Readonly<Record<string, ProposedMissionFields>>
}): MissionView[] =>
  missions.map((mission) => ({
    mission,
    attention: attention.filter((item) => item.mission_id === mission.id && item.state === 'open'),
    proposed: proposed?.[mission.id],
  }))

/** Refs an open attention row is about: what raised it, who asked and the mission it serves. */
export const attentionRefs = (attention: readonly St3.Attention[]): ReadonlySet<string> =>
  new Set(
    attention
      .filter((item) => item.state === 'open')
      .flatMap((item) => {
        const refs: string[] = [item.source_id]
        if (item.requester_id !== undefined) refs.push(item.requester_id)
        if (item.mission_id !== undefined) refs.push(item.mission_id)
        return refs
      }),
  )
const agentSubjectPair = ({
  agent,
  needsYou,
}: {
  readonly agent: Agent
  readonly needsYou: boolean
}): SubjectSummary[] => [
  {
    ref: agent.ref,
    title: agent.name,
    detail: agent.host,
    host: agent.host,
    ...(agent.harness === undefined ? {} : { harness: agent.harness }),
    icon: 'conversation',
    ...(agent.connected
      ? agent.activity === 'working'
        ? { status: 'live' as const }
        : {}
      : { status: 'unavailable' as const }),
    ...(needsYou ? { attention: true } : {}),
  },
  {
    ref: agent.terminal,
    title: agent.name,
    detail: `${agent.host} · terminal`,
    host: agent.host,
    icon: 'terminal',
    status: agent.connected ? 'live' : 'unavailable',
  },
]

/** Each observed family contributes navigation independently; absent input is not an empty feed. */
export const subjectList = ({
  fleet,
  missions,
  attention,
}: {
  readonly fleet?: Fleet
  readonly missions?: readonly St3.Mission[]
  readonly attention?: readonly St3.Attention[]
}): SubjectSummary[] => {
  const needsYou = attention === undefined ? undefined : attentionRefs(attention)
  return [
    ...(fleet?.agents.flatMap((agent) =>
      agentSubjectPair({ agent, needsYou: needsYou?.has(agent.ref) === true }),
    ) ?? []),
    ...(missions?.map(
      (mission): SubjectSummary => ({
        ref: mission.id,
        title: mission.title,
        detail: `mission · ${typeof mission.state === 'string' ? mission.state : mission.state.raw}`,
        icon: 'missions',
        ...(needsYou?.has(mission.id) ? { attention: true } : {}),
      }),
    ) ?? []),
    ...(attention?.map(
      (item): SubjectSummary => ({
        ref: item.id,
        title: item.title,
        detail: `${item.attention_kind} · ${item.state}`,
        icon: 'attention',
        ...(item.state === 'open' ? { attention: true } : {}),
      }),
    ) ?? []),
  ]
}

// oxlint-disable-next-line overeng/named-args -- Retained-identity array comparator; fixed positional Equivalence shape.
const sameItems = <T>(left: readonly T[], right: readonly T[]): boolean =>
  left.length === right.length && left.every((item, index) => item === right[index])

const sameKnown = <T>(left: Known<T>, right: Known<T>, equivalent: (left: T, right: T) => boolean = Object.is): boolean =>
  left._tag === 'Unknown' ? right._tag === 'Unknown' : right._tag === 'Known' && equivalent(left.value, right.value)
const sameUsage = Schema.toEquivalence(St3.UsageSummary)
const sameCheckout = Schema.toEquivalence(St3.AgentCheckout)

// oxlint-disable-next-line overeng/named-args -- Retained Agent projection comparator; fixed positional Equivalence shape.
const sameAgent = (a: Agent, b: Agent): boolean =>
  a.ref === b.ref &&
  a.terminal === b.terminal &&
  a.terminalDisabledReason === b.terminalDisabledReason &&
  a.name === b.name &&
  a.lifecycle._tag === b.lifecycle._tag &&
  a.host === b.host &&
  a.harness === b.harness &&
  a.activity === b.activity &&
  a.status === b.status &&
  a.state === b.state &&
  a.description === b.description &&
  sameKnown(a.lastActivityAt, b.lastActivityAt) &&
  sameKnown(a.startedAt, b.startedAt) &&
  sameKnown(a.endedAt, b.endedAt) &&
  sameKnown(a.usage, b.usage, sameUsage) &&
  sameKnown(a.checkout, b.checkout, sameCheckout) &&
  sameKnown(a.workspace, b.workspace) &&
  sameKnown(a.blockedOn, b.blockedOn) &&
  sameKnown(a.ask, b.ask) &&
  a.statusSince === b.statusSince &&
  a.mission === b.mission &&
  a.connected === b.connected

/**
 * Source-local projection memoization, not another copy of the source rows. Row identities are
 * the incremental boundary: unchanged rows keep their views, including across freshness changes.
 * Weak keys release evicted rows; only the latest output arrays are retained.
 */
export const createProjections = (): {
  readonly fleetFromAgents: typeof fleetFromAgents
  readonly missionViews: typeof missionViews
  readonly subjectList: typeof subjectList
} => {
  const agents = new WeakMap<St3.Agent, Agent>()
  const views = new WeakMap<St3.Mission, MissionView>()
  const agentSubjects = new WeakMap<Agent, readonly SubjectSummary[]>()
  const missionSubjects = new WeakMap<St3.Mission, SubjectSummary>()
  const attentionSubjects = new WeakMap<St3.Attention, SubjectSummary>()
  let previousRows: readonly St3.Agent[] | undefined
  let previousFleet: Fleet | undefined
  let previousViews: MissionView[] = []
  let previousSubjects: SubjectSummary[] = []
  return {
    fleetFromAgents: (rows) => {
      if (rows === previousRows && previousFleet !== undefined) return previousFleet
      const previousAgents = new Map(previousFleet?.agents.map((agent) => [agent.ref, agent]))
      const projected = rows.map((row) => {
        let agent = agents.get(row)
        if (agent === undefined) {
          const candidate = agentFromRow(row)
          const previous = previousAgents.get(row.id)
          agent = previous !== undefined && sameAgent(previous, candidate) ? previous : candidate
          agents.set(row, agent)
        }
        return agent
      })
      const connected = new Map<string, boolean>()
      for (const agent of projected)
        connected.set(agent.host, agent.connected || connected.get(agent.host) === true)
      const oldHosts = new Map(previousFleet?.hosts.map((host) => [host.id, host]))
      const hosts = [...connected].map(([id, live]): Host => {
        const old = oldHosts.get(id)
        return old?.connected === live ? old : { id, connected: live }
      })
      const fleet = {
        agents:
          previousFleet !== undefined && sameItems(previousFleet.agents, projected)
            ? previousFleet.agents
            : projected,
        hosts:
          previousFleet !== undefined && sameItems(previousFleet.hosts, hosts)
            ? previousFleet.hosts
            : hosts,
      }
      previousRows = rows
      if (
        previousFleet === undefined ||
        previousFleet.agents !== fleet.agents ||
        previousFleet.hosts !== fleet.hosts
      )
        previousFleet = fleet
      return previousFleet
    },
    missionViews: ({ missions, attention, proposed }) => {
      const open = new Map<string, St3.Attention[]>()
      for (const item of attention) {
        if (item.state !== 'open' || item.mission_id === undefined) continue
        const rows = open.get(item.mission_id)
        if (rows === undefined) open.set(item.mission_id, [item])
        else rows.push(item)
      }
      const next = missions.map((mission) => {
        const previous = views.get(mission)
        const cards = open.get(mission.id) ?? []
        const fields = proposed?.[mission.id]
        if (
          previous !== undefined &&
          previous.proposed === fields &&
          sameItems(previous.attention, cards)
        )
          return previous
        const view = { mission, attention: cards, proposed: fields }
        views.set(mission, view)
        return view
      })
      if (!sameItems(previousViews, next)) previousViews = next
      return previousViews
    },
    subjectList: ({ fleet, missions, attention }) => {
      const needsYou = attention === undefined ? undefined : attentionRefs(attention)
      const next: SubjectSummary[] = []
      for (const agent of fleet?.agents ?? []) {
        let subjects = agentSubjects.get(agent)
        const needed = needsYou?.has(agent.ref) === true
        if (subjects === undefined || (subjects[0]?.attention === true) !== needed) {
          const projected = agentSubjectPair({ agent, needsYou: needed })
          if (subjects?.[1] !== undefined) projected[1] = subjects[1]
          subjects = projected
          agentSubjects.set(agent, subjects)
        }
        next.push(...subjects)
      }
      for (const mission of missions ?? []) {
        let subject = missionSubjects.get(mission)
        const needed = needsYou?.has(mission.id) === true
        if (subject === undefined || (subject.attention === true) !== needed) {
          subject = {
            ref: mission.id,
            title: mission.title,
            detail: `mission · ${typeof mission.state === 'string' ? mission.state : mission.state.raw}`,
            icon: 'missions',
            ...(needed ? { attention: true } : {}),
          }
          missionSubjects.set(mission, subject)
        }
        next.push(subject)
      }
      for (const item of attention ?? []) {
        let subject = attentionSubjects.get(item)
        if (subject === undefined) {
          subject = {
            ref: item.id,
            title: item.title,
            detail: `${item.attention_kind} · ${item.state}`,
            icon: 'attention',
            ...(item.state === 'open' ? { attention: true } : {}),
          }
          attentionSubjects.set(item, subject)
        }
        next.push(subject)
      }
      if (!sameItems(previousSubjects, next)) previousSubjects = next
      return previousSubjects
    },
  }
}
