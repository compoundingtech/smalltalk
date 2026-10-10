/**
 * What the workspace body shows for the selected pane. The thread and terminal subjects have web surfaces; any
 * other pane states its own unavailability instead of repeating the transcript under a different tab.
 */
export type WorkspaceView =
  | { readonly _tag: 'Thread' }
  | { readonly _tag: 'Terminal'; readonly ref: TerminalRef }
  | { readonly _tag: 'ViewUnavailable'; readonly ref: string; readonly reason: string }

/** A terminal subject address, as `terminalSubjectForAgent` and the fleet projection produce it. */
export type TerminalRef = `terminal/${string}`
const isTerminalRef = (ref: string): ref is TerminalRef => ref.startsWith('terminal/')

export const workspaceView = (pane: { readonly ref: string } | undefined): WorkspaceView =>
  pane === undefined
    ? { _tag: 'Thread' }
    : isTerminalRef(pane.ref)
      ? { _tag: 'Terminal', ref: pane.ref }
      : { _tag: 'ViewUnavailable', ref: pane.ref, reason: 'This web client does not render this view yet.' }

export const workspaceViewNotice = (view: Extract<WorkspaceView, { _tag: 'ViewUnavailable' }>): string =>
  `View unavailable. ${view.reason}`

/** A conversation pane kept mounted after it was opened; `used` is its logical recency. */
export interface RetainedPane {
  readonly ref: string
  readonly name: string
  readonly used: number
}

/** Memory-only LRU over opened panes. Array order is insertion order and never changes for a
 * retained pane: moving a mounted subtree in the DOM discards its rendering state. */
export const retainPane = ({ panes, ref, name, limit }: {
  readonly panes: readonly RetainedPane[]
  readonly ref: string
  readonly name: string
  readonly limit: number
}): readonly RetainedPane[] => {
  const used = panes.reduce((latest, pane) => Math.max(latest, pane.used), 0) + 1
  if (panes.some(pane => pane.ref === ref)) return panes.map(pane => pane.ref === ref ? { ref, name, used } : pane)
  const next = [...panes, { ref, name, used }]
  if (next.length <= limit) return next
  const evicted = next.reduce((oldest, pane) => pane.used < oldest.used ? pane : oldest)
  return next.filter(pane => pane !== evicted)
}

