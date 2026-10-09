import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { geometryVars as g, motionVars as m } from '../composition-tokens.stylex'

export type IconName =
  | 'search' | 'plus' | 'pencil' | 'gear' | 'swap' | 'clock'
  | 'chevron-right' | 'chevron-down' | 'chevrons-left' | 'panel' | 'drawer'
  | 'attach' | 'copy' | 'send' | 'stop' | 'check' | 'x' | 'alert'
  | 'spinner' | 'folder' | 'message' | 'dot'
  | 'wrap' | 'whitespace' | 'pull-request' | 'stop-circle'

const PATHS: Record<IconName, React.ReactNode> = {
  search: <><circle cx="7" cy="7" r="4.5" /><path d="M10.5 10.5 14 14" /></>,
  plus: <path d="M8 3v10M3 8h10" />,
  pencil: <path d="M11.5 2.5 13.5 4.5 5 13H3v-2z" />,
  gear: <><circle cx="8" cy="8" r="2.25" /><path d="M8 1.75v1.5M8 12.75v1.5M1.75 8h1.5M12.75 8h1.5M3.6 3.6l1.06 1.06M11.34 11.34l1.06 1.06M12.4 3.6l-1.06 1.06M4.66 11.34 3.6 12.4" /></>,
  swap: <path d="M4 5h8l-2.5-2.5M12 11H4l2.5 2.5" />,
  clock: <><circle cx="8" cy="8" r="5.5" /><path d="M8 5v3l2 1.5" /></>,
  'chevron-right': <path d="M6 3.5 10.5 8 6 12.5" />,
  'chevron-down': <path d="M3.5 6 8 10.5 12.5 6" />,
  'chevrons-left': <path d="M9.5 3.5 5 8l4.5 4.5M13 3.5 8.5 8l4.5 4.5" />,
  panel: <><rect x="2" y="3" width="12" height="10" rx="1.5" /><path d="M10 3v10" /></>,
  drawer: <><rect x="2" y="4" width="12" height="8" rx="1.5" /><path d="M2 7.5h12M4.5 9.75h.01M6.5 9.75h2" /></>,
  attach: <path d="M10.5 5.5 5.8 10.2a1.9 1.9 0 0 0 2.7 2.7l4.9-4.9a3.4 3.4 0 0 0-4.8-4.8L3.4 8.4a4.9 4.9 0 0 0 6.9 6.9l3.2-3.2" />,
  copy: <><rect x="5.5" y="5.5" width="8" height="8" rx="1.5" /><path d="M10.5 5.5V4A1.5 1.5 0 0 0 9 2.5H4A1.5 1.5 0 0 0 2.5 4v5A1.5 1.5 0 0 0 4 10.5h1.5" /></>,
  send: <path d="M8 13V3M3.5 7.5 8 3l4.5 4.5" />,
  stop: <rect x="2" y="2" width="12" height="12" rx="1" fill="currentColor" stroke="none" />,
  check: <path d="M3 8.5 6.5 12 13 4.5" />,
  x: <path d="M4 4l8 8M12 4l-8 8" />,
  alert: <><path d="M8 2 14.5 13.5H1.5z" /><path d="M8 6.5v3M8 11.5h.01" /></>,
  spinner: <><path d="M8 1.5A6.5 6.5 0 1 1 1.5 8" /><path d="M8 4.5A3.5 3.5 0 1 0 11.5 8" /></>,
  folder: <path d="M2 4.5A1.5 1.5 0 0 1 3.5 3h2.6l1.4 1.8h5A1.5 1.5 0 0 1 14 6.3v5.2a1.5 1.5 0 0 1-1.5 1.5h-9A1.5 1.5 0 0 1 2 11.5z" />,
  message: <path d="M2.5 4.5A2 2 0 0 1 4.5 2.5h7a2 2 0 0 1 2 2v5a2 2 0 0 1-2 2H7l-3 2.5v-2.5h-.5a2 2 0 0 1-1-2z" />,
  dot: <circle cx="8" cy="8" r="3" />,
  wrap: <><path d="M2 3h12M2 7h9a3 3 0 0 1 0 6H7M9 11l-2 2 2 2M2 11h2" /></>,
  whitespace: <><path d="M9 14V2h4M12 2v12M9 2H6a3 3 0 0 0 0 6h3" /><circle cx="3" cy="12" r=".6" fill="currentColor" /></>,
  'pull-request': <><circle cx="4" cy="3" r="1.5" /><circle cx="4" cy="13" r="1.5" /><circle cx="12" cy="13" r="1.5" /><path d="M4 4.5v7M12 11.5V6a3 3 0 0 0-3-3H7M9 1 7 3l2 2" /></>,
  'stop-circle': <><circle cx="8" cy="8" r="6" /><rect x="5" y="5" width="6" height="6" rx=".5" fill="currentColor" stroke="none" /></>,
}

/** One 16px stroke icon set; stroke inherits currentColor, fill none. */
export function Icon({ name, size, scale = 'normal', spinning = false, label }: { name: IconName; size?: number; scale?: 'normal' | 'status'; spinning?: boolean; label?: string }) {
  return <svg
    width={size}
    height={size}
    viewBox="0 0 16 16"
    fill="none"
    stroke="currentColor"
    strokeWidth={1.5}
    strokeLinecap="round"
    strokeLinejoin="round"
    aria-hidden={label === undefined ? true : undefined}
    role={label === undefined ? undefined : 'img'}
    aria-label={label}
    {...stylex.props(styles.icon, scale === 'status' && styles.status, size !== undefined && styles.size(size), spinning && styles.spin)}
  >{PATHS[name]}</svg>
}

const styles = stylex.create({
  icon: { width: g.icon, height: g.icon, flexShrink: 0 },
  status: { width: g.status, height: g.status },
  size: (size: number) => ({ width: size, height: size }),
  spin: { animationName: stylex.keyframes({ to: { transform: 'rotate(360deg)' } }), animationDuration: m.spin, animationTimingFunction: m.linear, animationIterationCount: 'infinite', '@media (prefers-reduced-motion: reduce)': { animationName: 'none' } },
})
