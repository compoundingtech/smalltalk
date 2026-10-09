/**
 * What the workspace body shows for the selected pane. Only the thread has a web surface; any other pane states
 * its own unavailability instead of repeating the transcript under a different tab.
 */
export type WorkspaceView =
  | { readonly _tag: 'Thread' }
  | { readonly _tag: 'TerminalUnavailable'; readonly ref: string; readonly reason: string }
  | { readonly _tag: 'ViewUnavailable'; readonly ref: string; readonly reason: string }

export const workspaceView = (pane: { readonly ref: string } | undefined): WorkspaceView =>
  pane === undefined
    ? { _tag: 'Thread' }
    : pane.ref.startsWith('terminal/')
      ? { _tag: 'TerminalUnavailable', ref: pane.ref, reason: 'This web client has no terminal renderer yet.' }
      : { _tag: 'ViewUnavailable', ref: pane.ref, reason: 'This web client does not render this view yet.' }

export const workspaceViewNotice = (view: Exclude<WorkspaceView, { _tag: 'Thread' }>): string =>
  `${view._tag === 'TerminalUnavailable' ? 'Terminal unavailable' : 'View unavailable'}. ${view.reason}`

