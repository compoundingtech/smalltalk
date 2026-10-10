// The base kit ships no icon set; these are the workbench chrome glyphs (16px grid, 1.5 stroke, currentColor).

import type { ReactNode } from 'react'

/** Close glyph; tabs use the compact default size. */
export const CloseIcon = ({ size = 12 }: { readonly size?: number }) => (
  <Glyph size={size}>
    <path d="M4 4l8 8M12 4l-8 8" />
  </Glyph>
)
/** Expanded disclosure chevron. */
export const ChevronDownIcon = ({ size = 12 }: { readonly size?: number }) => (
  <Glyph size={size}>
    <path d="M4 6l4 4 4-4" />
  </Glyph>
)
/** Collapsed disclosure chevron. */
export const ChevronRightIcon = ({ size = 12 }: { readonly size?: number }) => (
  <Glyph size={size}>
    <path d="M6 4l4 4-4 4" />
  </Glyph>
)
/** Split the editor area to the right. */
export const SplitRightIcon = () => (
  <Glyph>
    <rect x="2" y="2.5" width="12" height="11" rx="1.5" />
    <path d="M8 2.5v11" />
  </Glyph>
)
/** Split the editor area downward. */
export const SplitDownIcon = () => (
  <Glyph>
    <rect x="2" y="2.5" width="12" height="11" rx="1.5" />
    <path d="M2 8h12" />
  </Glyph>
)
/** Overflow menu trigger. */
export const MoreIcon = () => (
  <Glyph>
    <circle cx="3.5" cy="8" r="0.5" fill="currentColor" />
    <circle cx="8" cy="8" r="0.5" fill="currentColor" />
    <circle cx="12.5" cy="8" r="0.5" fill="currentColor" />
  </Glyph>
)
/** Left dock visibility toggle. */
export const LeftDockIcon = () => (
  <Glyph>
    <rect x="2" y="2.5" width="12" height="11" rx="1.5" />
    <path d="M6 2.5v11" />
  </Glyph>
)
/** Right dock visibility toggle. */
export const RightDockIcon = () => (
  <Glyph>
    <rect x="2" y="2.5" width="12" height="11" rx="1.5" />
    <path d="M10 2.5v11" />
  </Glyph>
)
/** Bottom dock visibility toggle. */
export const BottomDockIcon = () => (
  <Glyph>
    <rect x="2" y="2.5" width="12" height="11" rx="1.5" />
    <path d="M2 10h12" />
  </Glyph>
)
/** Search and command palette trigger. */
export const SearchIcon = () => (
  <Glyph size={14}>
    <circle cx="7" cy="7" r="4.5" />
    <path d="M10.5 10.5L14 14" />
  </Glyph>
)

const Glyph = ({
  size = 16,
  children,
}: {
  readonly size?: number
  readonly children: ReactNode
}) => (
  <svg
    width={size}
    height={size}
    viewBox="0 0 16 16"
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
