// View-container icons, addressed by the string `icon` field of a contribution.

import type { ReactNode } from 'react'

/** A view-container glyph by contribution `icon` name; unknown names fall back to `resources`. */
export const ContainerIcon = ({
  name,
  size,
}: {
  readonly name: string
  readonly size?: number
}) => <Glyph {...(size === undefined ? {} : { size })}>{paths[name] ?? paths['resources']}</Glyph>

const paths: Readonly<Record<string, ReactNode>> = {
  folder: <path d="M2.5 6h6l2-2h7v12h-15z" />,
  folderPlus: (
    <>
      <path d="M2.5 6h6l2-2h7v12h-15z" />
      <path d="M10 8v6M7 11h6" />
    </>
  ),
  folderOpen: <path d="M2.5 7V4h6l2 2h7v3M2.5 7h15l-2 9h-13z" />,
  plus: <path d="M10 4v12M4 10h12" />,
  edit: <path d="m4 12 8-8 4 4-8 8H4zM10 6l4 4" />,
  close: <path d="m5 5 10 10M15 5 5 15" />,
  drag: (
    <>
      <circle cx="7" cy="5" r="1" />
      <circle cx="13" cy="5" r="1" />
      <circle cx="7" cy="10" r="1" />
      <circle cx="13" cy="10" r="1" />
      <circle cx="7" cy="15" r="1" />
      <circle cx="13" cy="15" r="1" />
    </>
  ),
  working: <path d="m8 3 7 7-7 7z" />,
  waiting: <path d="M7 4v12M13 4v12" />,
  idle: <circle cx="10" cy="10" r="6" />,
  unobserved: (
    <>
      <circle cx="10" cy="10" r="6" strokeDasharray="2 3" />
      <path d="M10 7v4M10 14h.01" />
    </>
  ),
  stale: (
    <>
      <circle cx="10" cy="10" r="7" />
      <path d="M10 5v6l3 2" />
    </>
  ),
  ended: (
    <>
      <circle cx="10" cy="10" r="7" />
      <path d="m6 6 8 8M14 6l-8 8" />
    </>
  ),
  pending: (
    <>
      <circle cx="10" cy="10" r="7" />
      <path d="M10 5v5H6" />
    </>
  ),
  retired: (
    <>
      <rect x="3" y="4" width="14" height="12" rx="1" />
      <path d="M3 8h14M8 11h4" />
    </>
  ),
  offline: (
    <>
      <circle cx="10" cy="10" r="6" />
      <path d="m4 16 12-12" />
    </>
  ),
  suspended: (
    <>
      <rect x="3" y="3" width="14" height="14" rx="2" />
      <path d="M7 6v8M13 6v8" />
    </>
  ),
  check: <path d="m4 10 4 4 8-9" />,
  inbox: (
    <>
      <path d="M3 4h14v12H3zM3 11h4l1 2h4l1-2h4" />
    </>
  ),
  agents: (
    <>
      <rect x="3" y="4" width="14" height="10" rx="2" />
      <path d="M7 17h6M10 14v3M7.5 9h.01M12.5 9h.01" />
    </>
  ),
  missions: (
    <>
      <path d="M4 17V3.5" />
      <path d="M4 4h10l-2 3.5L14 11H4" />
    </>
  ),
  attention: (
    <>
      <path d="M10 3a5 5 0 015 5v3l1.5 3h-13L5 11V8a5 5 0 015-5z" />
      <path d="M8.5 16.5a1.5 1.5 0 003 0" />
    </>
  ),
  resources: (
    <>
      <path d="M10 2.5l7 3.5-7 3.5-7-3.5 7-3.5z" />
      <path d="M3 10l7 3.5 7-3.5M3 14l7 3.5 7-3.5" />
    </>
  ),
  inspector: (
    <>
      <rect x="3" y="3" width="14" height="14" rx="2" />
      <path d="M7 7h6M7 10h6M7 13h3" />
    </>
  ),
  monitor: <path d="M3 15l4-5 3 3 4-6 3 4" />,
  events: <path d="M4 5h12M4 10h12M4 15h8" />,
  terminal: (
    <>
      <rect x="2.5" y="3.5" width="15" height="13" rx="2" />
      <path d="M6 8l2.5 2L6 12M10.5 12.5H14" />
    </>
  ),
  conversation: (
    <path d="M4 5.5A2.5 2.5 0 016.5 3h7A2.5 2.5 0 0116 5.5v5a2.5 2.5 0 01-2.5 2.5H9l-4 3.5V13a2.5 2.5 0 01-1-2z" />
  ),
  settings: (
    <>
      <circle cx="10" cy="10" r="2.5" />
      <path d="M10 2.5v2M10 15.5v2M2.5 10h2M15.5 10h2M4.7 4.7l1.4 1.4M13.9 13.9l1.4 1.4M4.7 15.3l1.4-1.4M13.9 6.1l1.4-1.4" />
    </>
  ),
  vista: (
    <>
      <rect x="2.5" y="3.5" width="15" height="13" rx="2" />
      <path d="M2.5 7h15M6 13l2.5-2.5 2 2L14 9" />
    </>
  ),
}

const Glyph = ({
  children,
  size = 20,
}: {
  readonly children: ReactNode
  readonly size?: number
}) => (
  <svg
    width={size}
    height={size}
    viewBox="0 0 20 20"
    fill="none"
    stroke="currentColor"
    strokeWidth="1.5"
    strokeLinecap="round"
    strokeLinejoin="round"
    aria-hidden="true"
  >
    {children}
  </svg>
)
