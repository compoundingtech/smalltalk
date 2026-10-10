import * as Native from '@smalltalk/st3-client/schema'
import type { WireSlice } from '@smalltalk/st3-scenarios/react'
import { DateTime, Option } from 'effect'
import { normalizePublicConversation } from './embrace-data/normalize-public-world'
import type { PublicTimelineEntry } from './embrace-data/public-world'
import { statuses, type AgentStatus, type SidebarAgentRow } from './sidebar/model'

const decodeAgent = Native.decodeUnknownSync(Native.Agent, 'strict')
const decodeAttention = Native.decodeUnknownSync(Native.Attention, 'strict')
const decodeMessage = Native.decodeUnknownSync(Native.Message, 'strict')
const decodeEntry = Native.decodeUnknownSync(Native.TimelineEntry, 'strict')
const decodeCapabilities = Native.decodeUnknownSync(Native.Capabilities, 'strict')
const unknown = { _tag: 'Unknown' } as const

export const projectRoster = (slice: WireSlice<'roster'>) => ({ loading: slice.loading, agents: slice.state.agents.map(agent => decodeAgent(agent)) })
export const projectAttention = (slice: WireSlice<'attention'>) => ({
  loading: slice.loading,
  cards: slice.state.attention.map(card => decodeAttention(card)),
  messages: slice.state.messages.map(wire => {
    const message = decodeMessage(wire)
    return { id: message.id, title: Option.getOrUndefined(message.title), content: message.content }
  }),
})

export function projectAgentRow(agent: Native.Agent, attention: readonly Native.Attention[]): SidebarAgentRow {
  const harnessState = Option.getOrUndefined(agent.harness_state)
  const status: AgentStatus = agent.observation === 'missing' ? 'unobserved'
    : agent.observation === 'stale' ? 'stale'
    : agent.state === 'suspended' ? 'suspended'
    : agent.state === 'failed' || Option.isSome(agent.fault) ? 'stale'
    : agent.state === 'stopped' ? 'ended'
    : agent.reachability === 'unreachable' ? 'offline'
    : agent.state === 'waiting' || harnessState === 'waiting' || Option.getOrUndefined(agent.blocked_on) === 'human' ? 'waiting'
    : agent.state === 'running' || harnessState === 'working' ? 'working'
    : agent.state === 'starting' || agent.state === 'desired' ? 'pending' : 'idle'
  const work = agent.current_work?.[0]
  const activity = Option.getOrUndefined(agent.last_activity_at)
  const since = Option.getOrUndefined(agent.since)
  const usage = Option.getOrUndefined(agent.usage)
  const needsMe = attention.some(card => card.state === 'open' && (card.source_id === agent.id || card.requester_id === agent.id || card.targets?.includes(agent.id)))
  return {
    ref: agent.id, id: agent.id.slice('agent/'.length), title: agent.name,
    description: work === undefined ? undefined : Option.getOrUndefined(work.goal) ?? Option.getOrUndefined(work.title),
    status, statusLabel: Option.getOrUndefined(agent.fault) ?? Option.getOrUndefined(agent.reason) ?? statuses[status].label,
    host: Option.getOrUndefined(agent.host_id) ?? 'local', harness: Option.getOrUndefined(agent.driver),
    model: usage?.context?.model, worktree: Option.getOrUndefined(agent.workspace),
    branch: Option.getOrUndefined(agent.checkout)?.branch,
    statusSince: since === undefined ? undefined : DateTime.toEpochMillis(since),
    lastActivityAt: activity === undefined ? undefined : DateTime.toEpochMillis(activity),
    needsMe,
    freshness: agent.observation === 'stale' ? 'stale' : agent.observation === 'missing' ? 'unobserved' : 'live',
    // Native usage has no sidebar lifetime/subagent scope; retain the app's Unknown projection.
    usage: unknown,
    duration: unknown,
    lastTurn: activity === undefined ? unknown : { _tag: 'Known', kind: 'activity', at: DateTime.toEpochMillis(activity) },
    childrenKnown: agent.subagents?.length === 0, children: [],
  }
}

/** The workshop's existing four-kind fold; no app-owned timeline implementation is copied. */
export function projectConversation(slice: WireSlice<'conversation'>) {
  return { loading: slice.loading, threads: slice.state.threads.map(thread => {
    const supported: PublicTimelineEntry[] = []
    const unprojected = new Set<string>()
    for (const raw of thread.items) {
      const entry = decodeEntry(raw)
      const common = { id: entry.id, sequence: entry.sequence, revision: entry.revision, final: entry.final, role: entry.role, timestamp: DateTime.formatIso(entry.timestamp) }
      switch (entry.type) {
        case 'message': supported.push({ ...common, type: 'message', body: { ...entry.body, reply_to: Option.getOrNull(entry.body.reply_to) } }); break
        case 'content':
          if (entry.body.text === undefined) unprojected.add('content without plain text')
          else supported.push({ ...common, type: 'content', body: { ...entry.body, text: entry.body.text } })
          break
        case 'tool_call': supported.push({ ...common, type: 'tool_call', body: entry.body }); break
        case 'tool_result': supported.push({ ...common, type: 'tool_result', body: entry.body }); break
        default: unprojected.add(typeof entry.type === 'string' ? entry.type : entry.type.raw)
      }
    }
    return { agent: thread.agent, unprojected: [...unprojected], items: normalizePublicConversation({ agentId: thread.agent, agentLabel: thread.agent, sessionId: thread.session_id, items: supported }) }
  }) }
}

export function projectSync(slice: WireSlice<'sync'>, anchor: number, now: number) {
  decodeCapabilities(slice.state.capabilities)
  return Object.entries(slice.status).map(([surface, status]) => {
    const expectation = slice.state.expected.filter(item => item.surface === surface && item.at_ms <= now - anchor).at(-1)
    const observedAt = expectation === undefined ? now : anchor + expectation.at_ms
    return { surface, status, observedAt }
  })
}
