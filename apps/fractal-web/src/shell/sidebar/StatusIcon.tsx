import * as stylex from '@stylexjs/stylex'
import type { ReactNode } from 'react'

import { tokens } from '../../ui-compat/tokens.stylex.ts'

import type { Agent, Feed } from '../../data/source.ts'

/** Accessible labels and visual tones for observed agent statuses. */
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
/** Sidebar status derived from an agent observation and fleet freshness. */
export type AgentStatus = keyof typeof statuses
/** Resolves status without treating unavailable or stale observations as live state. */
export const agentStatus = ({
  agent,
  fleet,
}: {
  readonly agent: Agent | undefined
  readonly fleet: Feed<unknown>
}): AgentStatus =>
  agent === undefined
    ? 'unobserved'
    : fleet._tag === 'Observed' && fleet.freshness === 'stale'
      ? 'stale'
      : agent.state === 'retired'
        ? 'retired'
        : agent.state === 'suspended'
          ? 'suspended'
          : agent.state === 'stopped' || agent.state === 'ended'
            ? 'ended'
            : agent.state === 'desired' || agent.state === 'starting'
              ? 'pending'
              : !agent.connected
                ? 'offline'
                : agent.activity === 'errored'
                  ? 'stale'
                  : agent.activity

const paths: Record<AgentStatus, ReactNode> = {
  working: (
    <>
      <circle cx="10" cy="10" r="7" opacity=".45" />
      <path d="M10 3a7 7 0 0 1 7 7" strokeWidth="1.75" />
    </>
  ),
  waiting: (
    <>
      <circle cx="10" cy="10" r="7" />
      <path d="M7.7 7.5a2.3 2.3 0 1 1 3.9 1.7c-1 .6-1.6 1.1-1.6 2.3M10 14h.01" />
    </>
  ),
  idle: <path d="M16.5 11.5A7 7 0 0 1 8.5 3.5a7 7 0 1 0 8 8Z" />,
  pending: (
    <>
      <path d="M6 3h8M6 17h8M7 3v3l6 8v3M13 3v3l-6 8v3" />
    </>
  ),
  stale: (
    <>
      <circle cx="10" cy="10" r="7" strokeDasharray="2 2" />
      <path d="M10 6v4l3 2" />
    </>
  ),
  offline: (
    <>
      <path d="M3 8a11 11 0 0 1 14 0M6 11a6 6 0 0 1 8 0M9 14a2 2 0 0 1 2 0M3 3l14 14" />
    </>
  ),
  ended: (
    <>
      <circle cx="10" cy="10" r="7" />
      <rect x="7" y="7" width="6" height="6" rx=".5" fill="currentColor" stroke="none" />
    </>
  ),
  suspended: (
    <>
      <circle cx="10" cy="10" r="7" />
      <path d="M8 7v6M12 7v6" />
    </>
  ),
  retired: (
    <>
      <rect x="3" y="4" width="14" height="3" rx="1" />
      <path d="M4 7v9h12V7M8 10h4" />
    </>
  ),
  unobserved: (
    <>
      <circle cx="10" cy="10" r="7" strokeDasharray="2 3" />
      <path d="M10 7v4M10 14h.01" />
    </>
  ),
}
const spin = stylex.keyframes({ to: { transform: 'rotate(360deg)' } })
const styles = stylex.create({
  icon: { display: 'inline-flex', flexShrink: 0, color: tokens['--ds-gray-900'] },
  green: { color: tokens['--ds-green-900'] },
  amber: { color: tokens['--ds-amber-900'] },
  spinner: {
    animationName: spin,
    animationDuration: '1s',
    animationTimingFunction: 'linear',
    animationIterationCount: 'infinite',
    animationPlayState: { default: 'running', '@media (prefers-reduced-motion: reduce)': 'paused' },
  },
})
/** Displays an accessible status glyph with optional stopped-session treatment. */
export const StatusIcon = ({
  status,
  endedGlyph = 'stop',
}: {
  readonly status: AgentStatus
  readonly endedGlyph?: 'stop' | 'flag'
}) => (
  <span
    role="img"
    aria-label={statuses[status].label}
    title={statuses[status].label}
    {...stylex.props(
      styles.icon,
      statuses[status].tone === 'green' && styles.green,
      statuses[status].tone === 'amber' && styles.amber,
    )}
  >
    <svg
      aria-hidden="true"
      width="16"
      height="16"
      viewBox="0 0 20 20"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.5"
      strokeLinecap="round"
      strokeLinejoin="round"
      {...stylex.props(status === 'working' && styles.spinner)}
    >
      {status === 'ended' && endedGlyph === 'flag' ? (
        <path d="M5 17V3h10l-2 4 2 4H5" />
      ) : (
        paths[status]
      )}
    </svg>
  </span>
)

/** Formats elapsed activity or status time compactly for agent surfaces. */
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
