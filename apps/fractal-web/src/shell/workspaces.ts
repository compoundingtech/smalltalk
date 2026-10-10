import type { Attention } from '@smalltalk/st3-client/schema'
import { Schema } from 'effect'

import { attentionRefs } from '../data/projections.ts'
import { SubjectAddress } from '../resources/contract.ts'
import type { SubjectSummary } from './context.tsx'
import {
  subjectKey,
  defaultResourcePanel,
  resourcePanelBounds,
  monitorDetailBounds,
  type ResourcePanelState,
  initialState,
  reduceLayout,
  type EditorEntry,
  type WorkbenchAction,
  type WorkbenchState,
} from './state.ts'

/** Session identity is shared by conversation and terminal, never by their surface lifetime. */
export const sessionRef = (ref: string): string | undefined =>
  ref.startsWith('agent/')
    ? ref
    : ref.startsWith('terminal/')
      ? `agent/${ref.slice('terminal/'.length)}`
      : undefined

/** User-local pane state; the sidebar and bottom panel belong to the window, not a session. */
export type WorkspaceLayout = Omit<WorkbenchState, 'docks'>

/** A live session projection plus its local pane and read state. */
export interface Workspace {
  readonly id: string
  readonly title: string
  readonly host: string
  readonly layout: WorkspaceLayout
  readonly notifications: ReadonlyArray<SubjectSummary>
  readonly unread: ReadonlyArray<string>
}

/** Window-owned selection, pane layouts, sidebar/panel state and notification read markers. */
export interface WindowState {
  readonly selected: string
  readonly layouts: Readonly<Record<string, WorkspaceLayout>>
  readonly docks: WorkbenchState['docks']
  readonly resources: ResourcePanelState
  readonly monitorDetailSize: number
  readonly read: ReadonlySet<string>
}

/** Navigation or layout changes initiated by workbench chrome. */
export type WindowAction =
  | { readonly _tag: 'SelectWorkspace'; readonly id: string }
  | { readonly _tag: 'ReadInbox'; readonly ref: string }
  | { readonly _tag: 'Resources'; readonly change: Partial<ResourcePanelState> }
  | { readonly _tag: 'ResizeMonitorDetail'; readonly size: number }
  | { readonly _tag: 'Layout'; readonly action: WorkbenchAction; readonly workspace?: string }

/** Seeds only local state; live titles, membership and attention remain source projections. */
export const createWindow = ({
  seed,
  subjects,
  attention,
}: {
  readonly seed: WorkbenchState
  readonly subjects: ReadonlyArray<SubjectSummary>
  readonly attention: readonly Attention[]
}): WindowState => {
  const seededSession = Object.values(seed.groups)
    .flatMap((pane) => pane.editors)
    .map((entry) => sessionRef(entry.input.ref))
    .find((ref) => ref !== undefined)
  const selected =
    seededSession ??
    subjects.find((subject) => subject.ref.startsWith('agent/'))?.ref ??
    'workspace/local'
  const layout = withoutDocks(seed)
  // Only a live session's workspace drops other sessions' surfaces; a local fallback keeps the seed.
  const groups = subjects.some((subject) => subject.ref === selected)
    ? Object.fromEntries(
        Object.entries(layout.groups).map(([id, pane]) => {
          const editors = pane.editors.filter(
            (entry) =>
              sessionRef(entry.input.ref) === undefined || sessionRef(entry.input.ref) === selected,
          )
          return [
            id,
            {
              editors,
              active: editors.some((entry) => subjectKey(entry.input) === pane.active)
                ? pane.active
                : editors[0] === undefined
                  ? null
                  : subjectKey(editors[0].input),
            },
          ]
        }),
      )
    : layout.groups
  const window: WindowState = {
    selected,
    layouts: { [selected]: { ...layout, groups } },
    docks: seed.docks,
    resources: defaultResourcePanel,
    monitorDetailSize: monitorDetailBounds.default,
    read: new Set(),
  }
  return markRead({
    window,
    workspace: projectWorkspaces({ window, subjects, attention }).find(
      (workspace) => workspace.id === selected,
    ),
  })
}

/** Session metadata and unread card identities derived from current rows and local read markers. */
export const projectWorkspaces = ({
  window,
  subjects,
  attention,
  previous,
  previousLayouts,
}: {
  readonly window: Pick<WindowState, 'layouts' | 'docks' | 'read'> & { readonly selected?: string }
  readonly subjects: ReadonlyArray<SubjectSummary>
  readonly attention: readonly Attention[]
  readonly previous?: ReadonlyArray<Workspace>
  readonly previousLayouts?: WindowState['layouts']
}): ReadonlyArray<Workspace> => {
  const sessions = subjects.filter((subject) => subject.ref.startsWith('agent/'))
  const metadata = sessions.map((session) => ({
    id: session.ref,
    title: session.title,
    host: session.host ?? 'local',
  }))
  if (window.selected !== undefined && !metadata.some((session) => session.id === window.selected))
    metadata.push({ id: window.selected, title: 'Local workspace', host: 'local' })
  const attentionSubjects = subjects.filter((subject) => subject.attention === true)
  const old = new Map(previous?.map((workspace) => [workspace.id, workspace]))
  const next = metadata.map(({ id, title, host }): Workspace => {
    const before = old.get(id)
    const layout =
      window.layouts[id] ??
      (previousLayouts?.[id] === undefined ? before?.layout : undefined) ??
      sessionLayout({ ref: id, docks: window.docks })
    const refs = new Set<string>(
      Object.values(layout.groups).flatMap((pane) => pane.editors.map((entry) => entry.input.ref)),
    )
    const notifications = attentionSubjects.filter(
      (subject) => sessionRef(subject.ref) === id || refs.has(subject.ref),
    )
    const cards = attention.filter(
      (item) =>
        item.state === 'open' &&
        [item.source_id, item.requester_id, item.mission_id].some(
          (ref) => ref !== undefined && (sessionRef(ref) === id || refs.has(ref)),
        ),
    )
    const represented = attentionRefs(cards)
    const unread = [
      ...cards.filter((item) => !window.read.has(item.id)).map((item) => item.id),
      ...notifications
        .filter((subject) => !represented.has(subject.ref) && !window.read.has(subject.ref))
        .map((subject) => subject.ref),
    ]
    if (
      before !== undefined &&
      before.title === title &&
      before.host === host &&
      before.layout === layout &&
      sameItems(before.notifications, notifications) &&
      sameItems(before.unread, unread)
    )
      return before
    return { id, title, host, layout, notifications, unread }
  })
  return previous !== undefined && sameItems(previous, next) ? previous : next
}

// oxlint-disable-next-line overeng/named-args -- Retained-identity array comparator; fixed positional Equivalence shape.
const sameItems = <T>(left: readonly T[], right: readonly T[]): boolean =>
  left.length === right.length && left.every((item, index) => item === right[index])

/** One window's incremental navigation projection; local layouts remain the sole pane authority. */
export const createWorkspaceProjection = (): typeof projectWorkspaces => {
  let previous: ReadonlyArray<Workspace> | undefined
  let previousLayouts: WindowState['layouts'] | undefined
  return (input) => {
    previous = projectWorkspaces({
      ...input,
      ...(previous === undefined ? {} : { previous }),
      ...(previousLayouts === undefined ? {} : { previousLayouts }),
    })
    previousLayouts = input.window.layouts
    return previous
  }
}

/** Applies pane transitions to one workspace and dock transitions to the single window owner. */
export const reduceWindow = ({
  window,
  action,
  subjects,
  attention,
}: {
  readonly window: WindowState
  readonly action: WindowAction
  readonly subjects: ReadonlyArray<SubjectSummary>
  readonly attention: readonly Attention[]
}): WindowState => {
  if (action._tag === 'ResizeMonitorDetail')
    return {
      ...window,
      monitorDetailSize: Math.max(
        monitorDetailBounds.min,
        Math.min(monitorDetailBounds.max, action.size),
      ),
    }
  if (action._tag === 'Resources') {
    const resources = { ...window.resources, ...action.change }
    return {
      ...window,
      resources: {
        ...resources,
        size: Math.max(resourcePanelBounds.min, Math.min(resourcePanelBounds.max, resources.size)),
      },
    }
  }
  if (action._tag === 'ReadInbox')
    return window.read.has(action.ref)
      ? window
      : { ...window, read: new Set([...window.read, action.ref]) }
  const workspaces = projectWorkspaces({ window, subjects, attention })
  const id = action._tag === 'SelectWorkspace' ? action.id : (action.workspace ?? window.selected)
  const workspace =
    workspaces.find((candidate) => candidate.id === id) ??
    (action._tag === 'Layout' && action.workspace === undefined ? workspaces[0] : undefined)
  if (workspace === undefined) return window
  if (action._tag === 'SelectWorkspace')
    return markRead({
      window: {
        ...window,
        selected: workspace.id,
        layouts:
          window.layouts[workspace.id] === undefined
            ? { ...window.layouts, [workspace.id]: workspace.layout }
            : window.layouts,
      },
      workspace,
    })
  if (action.action._tag === 'OpenTerminal' && workspace.id.startsWith('agent/')) {
    return reduceWindow({
      window,
      subjects,
      attention,
      action: {
        _tag: 'Layout',
        workspace: workspace.id,
        action:
          action.action.side === undefined
            ? { _tag: 'ShowView', dock: 'panel', view: 'workspace.terminal' }
            : {
                _tag: 'Open',
                input: {
                  ref: `terminal/${workspace.id.slice('agent/'.length)}`,
                  presentation: 'detail',
                },
                side: action.action.side,
              },
      },
    })
  }
  const previousLayout = { ...workspace.layout, docks: window.docks }
  const layout = reduceLayout({
    state: previousLayout,
    action: action.action,
  })
  // Opening the already-focused editor is not a layout transition. Keep the
  // saved pane identity so a cached sidebar activation only reveals Activity.
  if (layout === previousLayout && window.layouts[workspace.id] !== undefined)
    return window.selected === workspace.id ? window : { ...window, selected: workspace.id }
  const globalDock =
    action.action._tag === 'ToggleDock' ||
    action.action._tag === 'ShowView' ||
    action.action._tag === 'ResizeDock'
  // Individual inbox navigation can open a pane without reading its sibling cards.
  // Bulk read belongs only to explicit workspace selection (and the initial selected workspace).
  return {
    ...window,
    selected: workspace.id,
    docks: layout.docks,
    layouts: globalDock
      ? window.layouts
      : { ...window.layouts, [workspace.id]: withoutDocks(layout) },
  }
}

const withoutDocks = ({ docks: _docks, ...layout }: WorkbenchState): WorkspaceLayout => layout

const markRead = ({
  window,
  workspace,
}: {
  readonly window: WindowState
  readonly workspace: Workspace | undefined
}): WindowState =>
  workspace === undefined || workspace.unread.length === 0
    ? window
    : { ...window, read: new Set([...window.read, ...workspace.unread]) }

const sessionLayout = ({
  ref,
  docks,
}: {
  readonly ref: string
  readonly docks: WorkbenchState['docks']
}): WorkspaceLayout => {
  if (!ref.startsWith('agent/')) return withoutDocks(initialState(docks))
  const conversation: EditorEntry = {
    input: Schema.decodeSync(SubjectAddress)({ ref, presentation: 'detail' }),
  }
  return {
    ...withoutDocks(initialState(docks)),
    groups: {
      g0: { editors: [conversation], active: subjectKey(conversation.input) },
    },
  }
}
