export { matchPositions } from './fuzzy.ts'
export type SidebarUsage = { readonly _tag: 'Known'; readonly scope: '24h-root-and-subagents' | 'lifetime-incl-subagents'; readonly tokens: number; readonly usd: number } | { readonly _tag: 'Unknown' }
export type SidebarDuration = { readonly _tag: 'Known'; readonly scope: '24h-activity-span' | 'lifetime'; readonly ms: number } | { readonly _tag: 'Unknown' }
export type SidebarLastTurn = { readonly _tag: 'Known'; readonly kind: 'turn-completed' | 'activity'; readonly at: number } | { readonly _tag: 'Unknown' }
export interface SidebarReported {
  readonly model?: string
  /** Producers may provide both scoped observations; the row prefers an explicit lifetime fact. */
  readonly usage?: SidebarUsage | readonly SidebarUsage[]
  readonly duration?: SidebarDuration | readonly SidebarDuration[]
  /** Turn completion takes precedence over generic activity, which remains explicitly labelled. */
  readonly lastTurn?: SidebarLastTurn | readonly SidebarLastTurn[]
  readonly worktree?: string
  readonly branch?: string
  readonly pullRequest?: { readonly ref: string; readonly title: string; readonly number: number; readonly url?: string; readonly state: string }
}
export interface SidebarAgentRow extends SidebarReported {
  readonly ref: string
  /** Host workspace identity; canonical refs remain the projection/map keys. */
  readonly id: string
  readonly parentRef?: string
  readonly title: string
  readonly description?: string
  readonly subagent?: SidebarSubagent
  readonly harness?: string
  readonly terminal?: string
  readonly mission?: string
  readonly status: AgentStatus
  readonly statusLabel: string
  readonly host: string
  /** Exact metadata copied from the authoritative Agent projection, never duration/turn aliases. */
  readonly statusSince?: number
  readonly lastActivityAt?: number
  readonly needsMe?: boolean
  readonly freshness: 'live' | 'stale' | 'unobserved'
  readonly unread?: number
  readonly usage: SidebarUsage
  readonly duration: SidebarDuration
  readonly lastTurn: SidebarLastTurn
  readonly childrenKnown?: boolean
  readonly children: readonly SidebarAgentRow[]
}


export interface SidebarActions {
  readonly prefetch?: (ref: string) => void
  readonly details?: (ref: string) => void
  /** Terminal actions retain the owning host workspace, including nonselected rows. */
  readonly terminal?: (ref: string, workspaceId?: string) => void
  /** Owner uses the host workspace ID, without the canonical agent/ prefix. */
  readonly resource?: (ref: string, workspaceId?: string) => void
  readonly select?: (id: string, parentId?: string) => void
}

export interface SidebarSubagent { readonly id: string; readonly subagent_type?: string; readonly work_id?: string; readonly session_id?: string; readonly started_at?: string; readonly lease_expires_at: string }
export const statuses = {
  working: { label: 'Working', tone: 'green' },
  waiting: { label: 'Needs you', tone: 'amber' },
  idle: { label: 'Idle', tone: 'gray' },
  pending: { label: 'Pending', tone: 'gray' },
  stale: { label: 'Stale observation', tone: 'gray' },
  offline: { label: 'Offline', tone: 'gray' },
  ended: { label: 'Ended', tone: 'gray' },
  suspended: { label: 'Suspended', tone: 'gray' },
  retired: { label: 'Retired', tone: 'gray' },
  unobserved: { label: 'Not observed', tone: 'gray' },
} as const
export type AgentStatus = keyof typeof statuses
export const compactTime = ({ at, now }: { readonly at: number; readonly now: number }): string => {
  const seconds = Math.max(0, Math.floor((now - at) / 1000))
  return seconds < 60
    ? `${seconds}s`
    : seconds < 3600
      ? `${Math.floor(seconds / 60)}m`
      : seconds < 86400
        ? `${Math.floor(seconds / 3600)}h`
        : `${Math.floor(seconds / 86400)}d`
}
