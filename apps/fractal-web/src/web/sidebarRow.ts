import type { SidebarAgentRowData, SidebarAgentStatus } from '@smalltalk/fractal-ui/assistant-ui'
import type { Agent } from '../data/source.ts'

/** Presentation of one actual roster observation. No per-row reads or inferred usage. */
export const sidebarRow = ({ agent, stale, now }: {
  readonly agent: Agent; readonly stale: boolean; readonly now: number
}): SidebarAgentRowData => {
  const status: SidebarAgentStatus = stale ? 'stale' : agent.state === 'retired' ? 'retired'
    : agent.state === 'suspended' ? 'suspended'
    : agent.state === 'stopped' || agent.state === 'ended' ? 'ended'
    : agent.state === 'desired' || agent.state === 'starting' ? 'pending'
    : !agent.connected ? 'offline' : agent.activity === 'errored' ? 'stale' : agent.activity
  const validTimestamp = (value: number | undefined) => value !== undefined && Number.isFinite(value) &&
    Number.isFinite(now) && value <= now && value >= 0 && value <= 8640000000000000 ? value : undefined
  const lastActivityAt = validTimestamp(agent.lastActivityAt)
  const statusSince = validTimestamp(agent.statusSince)
  const knownStatuses = ['idle', 'working', 'waiting', 'errored', 'retired', 'suspended', 'stopped', 'ended', 'desired', 'starting', 'offline']
  const statusLabel = knownStatuses.includes(agent.status) ? agent.status : 'Not observed'
  return {
    ref: agent.ref, id: agent.ref, title: agent.name, host: agent.host, status,
    statusLabel: stale ? 'Last verified · ' + statusLabel : !agent.connected ? 'Offline · ' + statusLabel : statusLabel,
    freshness: stale ? 'stale' : 'live', description: agent.description, harness: agent.harness,
    terminal: agent.terminal || undefined, mission: agent.mission, lastActivityAt, statusSince,
    // Roster has no authoritative unread count or attention request. No signal is fabricated.
    childrenKnown: false, children: [], usage: { _tag: 'Unknown' }, duration: { _tag: 'Unknown' },
    lastTurn: lastActivityAt === undefined ? { _tag: 'Unknown' } : { _tag: 'Known', kind: 'activity', at: lastActivityAt },
  }
}
