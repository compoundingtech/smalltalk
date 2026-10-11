// Workbench layout state: independently focused subject presentations, a split tree and
// controlled docks. The window persistence boundary owns stored-format conversion.

import { Schema } from 'effect'

import { SubjectAddress } from '../resources/contract.ts'

/** Editor-group identity within one layout (`g0`, `g1`, …). */
export const GroupId = Schema.String
export type GroupId = typeof GroupId.Type

/** One open surface in a group. */
export const EditorEntry = Schema.Struct({ input: SubjectAddress })
export type EditorEntry = typeof EditorEntry.Type

/** A leaf of the split tree: one editor group. */
export interface GroupLeaf {
  readonly _tag: 'group'
  readonly id: GroupId
}
/** A split of the editor area into children laid out along `dir`. */
export interface SplitBranch {
  readonly _tag: 'split'
  readonly dir: 'row' | 'col'
  readonly children: ReadonlyArray<SplitNode>
  /** Fractions summing to 1, one per child. */
  readonly sizes: ReadonlyArray<number>
}
/** The editor area: a tree of splits over groups. */
export type SplitNode = GroupLeaf | SplitBranch

/** Recursive codec of the split tree. */
export const SplitNode: Schema.Codec<SplitNode> = Schema.Union([
  Schema.TaggedStruct('group', { id: GroupId }),
  Schema.TaggedStruct('split', {
    dir: Schema.Literals(['row', 'col']),
    children: Schema.Array(Schema.suspend((): Schema.Codec<SplitNode> => SplitNode)),
    sizes: Schema.Array(Schema.Finite),
  }),
])

/** An editor group: its surfaces in tab order and the active one. */
export const Group = Schema.Struct({
  editors: Schema.Array(EditorEntry),
  /** Entry key (`subjectKey`) of the active surface, or null for an empty group. */
  active: Schema.NullOr(Schema.String),
})
export type Group = typeof Group.Type

/** Where a dock sits: the navigation sidebar or bottom panel. */
export const DockLocation = Schema.Literals(['primary', 'panel'])
export type DockLocation = typeof DockLocation.Type

/** Visibility, size and shown container of one dock. */
export const DockState = Schema.Struct({
  visible: Schema.Boolean,
  /** Pixels: width for side docks, height for the panel dock. */
  size: Schema.Finite,
  /** View container id shown in the dock. */
  activeView: Schema.NullOr(Schema.String),
})
export type DockState = typeof DockState.Type

/** Window-owned resource inspector shared by all agent editors. */
export const ResourcePanelState = Schema.Struct({
  expanded: Schema.Boolean,
  size: Schema.Finite,
})
export type ResourcePanelState = typeof ResourcePanelState.Type
/** Allowed widths for the window-owned resource inspector. */
export const resourcePanelBounds = { min: 220, max: 560, default: 288 } as const
/** Collapsed resource inspector state for a new window. */
export const defaultResourcePanel: ResourcePanelState = {
  expanded: false,
  size: resourcePanelBounds.default,
}
/** Allowed widths for the window-owned account detail panel. */
export const monitorDetailBounds = { min: 280, max: 640, default: 380 } as const

/** Serializable pane layout and controlled dock state, opaque to contributed features. */
export const LayoutJson = Schema.Struct({
  version: Schema.Literal(2),
  editorArea: SplitNode,
  groups: Schema.Record(GroupId, Group),
  focusedGroup: GroupId,
  docks: Schema.Struct({ primary: DockState, panel: DockState }),
  nextGroup: Schema.Int,
})
export type WorkbenchState = typeof LayoutJson.Type

/** Identity of an open surface: one subject may have several presentations. */
export const subjectKey = (address: SubjectAddress): string =>
  `${address.presentation}|${address.ref}`

/** Edge of a group a dragged surface is dropped on. */
export type SplitZone = 'left' | 'right' | 'top' | 'bottom'

/** Every layout transition the reducer handles. */
export type WorkbenchAction =
  | { readonly _tag: 'Open'; readonly input: SubjectAddress; readonly side?: boolean }
  | { readonly _tag: 'OpenTerminal'; readonly side?: boolean }
  | { readonly _tag: 'Activate'; readonly group: GroupId; readonly key: string }
  | { readonly _tag: 'Close'; readonly group: GroupId; readonly key: string }
  | {
      readonly _tag: 'Move'
      readonly from: GroupId
      readonly key: string
      readonly to: GroupId
      readonly index: number
    }
  | {
      readonly _tag: 'MoveToSplit'
      readonly from: GroupId
      readonly key: string
      readonly target: GroupId
      readonly zone: SplitZone
    }
  | { readonly _tag: 'Split'; readonly dir: 'row' | 'col' }
  | {
      readonly _tag: 'Resize'
      readonly path: ReadonlyArray<number>
      readonly sizes: ReadonlyArray<number>
    }
  | { readonly _tag: 'FocusGroup'; readonly group: GroupId }
  | { readonly _tag: 'FocusGroupAt'; readonly index: number }
  | { readonly _tag: 'Cycle'; readonly delta: 1 | -1 }
  | { readonly _tag: 'MoveActive'; readonly delta: 1 | -1 }
  | { readonly _tag: 'CloseActive' }
  | { readonly _tag: 'ToggleDock'; readonly dock: DockLocation }
  | { readonly _tag: 'ShowView'; readonly dock: DockLocation; readonly view: string | null }
  | { readonly _tag: 'ResizeDock'; readonly dock: DockLocation; readonly size: number }

/** Groups in visual order (depth-first), which `Mod-1..9` indexes. */
export const groupOrder = (node: SplitNode): GroupId[] =>
  node._tag === 'group' ? [node.id] : node.children.flatMap(groupOrder)

/** The single layout transition function. */
export const reduceLayout = ({
  state,
  action,
}: {
  readonly state: WorkbenchState
  readonly action: WorkbenchAction
}): WorkbenchState => {
  switch (action._tag) {
    case 'OpenTerminal': {
      const group = state.groups[state.focusedGroup]
      const ref = group?.editors.find((entry) => subjectKey(entry.input) === group.active)?.input
        .ref
      if (ref === undefined || !ref.startsWith('agent/')) return state
      if (action.side === undefined)
        return reduceLayout({
          state,
          action: { _tag: 'ShowView', dock: 'panel', view: 'workspace.terminal' },
        })
      return reduceLayout({
        state,
        action: {
          _tag: 'Open',
          input: { ref: `terminal/${ref.slice('agent/'.length)}`, presentation: 'detail' },
          ...(action.side === undefined ? {} : { side: action.side }),
        },
      })
    }
    case 'Open': {
      // An editor open anywhere is focused where it is ("opening an open subject focuses its tab").
      const key = subjectKey(action.input)
      const owner = Object.entries(state.groups).find(([, g]) =>
        g.editors.some((e) => subjectKey(e.input) === key),
      )?.[0]
      if (owner !== undefined && action.side !== true)
        return openIn({ state, groupId: owner, input: action.input })
      if (action.side === true) {
        const order = groupOrder(state.editorArea)
        const next = order[order.indexOf(state.focusedGroup) + 1]
        if (next !== undefined) return openIn({ state, groupId: next, input: action.input })
        const [withNew, fresh] = newGroup(state)
        const split = {
          ...withNew,
          editorArea: insertBeside({
            node: withNew.editorArea,
            target: state.focusedGroup,
            fresh,
            dir: 'row',
            after: true,
          }),
        }
        return openIn({ state: split, groupId: fresh, input: action.input })
      }
      return openIn({ state, groupId: state.focusedGroup, input: action.input })
    }
    case 'Activate': {
      const group = state.groups[action.group]
      if (group === undefined) return state
      return {
        ...withGroup({ state, id: action.group, group: { ...group, active: action.key } }),
        focusedGroup: action.group,
      }
    }
    case 'Close': {
      const group = state.groups[action.group]
      if (group === undefined) return state
      return pruneGroup({
        state: withGroup({
          state,
          id: action.group,
          group: removeEditor({ group, key: action.key }),
        }),
        id: action.group,
      })
    }
    case 'CloseActive': {
      const active = state.groups[state.focusedGroup]?.active
      return active == null
        ? state
        : reduceLayout({ state, action: { _tag: 'Close', group: state.focusedGroup, key: active } })
    }
    case 'Move': {
      const from = state.groups[action.from]
      const entry = from?.editors.find((e) => subjectKey(e.input) === action.key)
      if (from === undefined || entry === undefined) return state
      if (action.from === action.to) {
        return {
          ...withGroup({
            state,
            id: action.to,
            group: insertEditor({ group: from, entry, index: action.index }),
          }),
          focusedGroup: action.to,
        }
      }
      const to = state.groups[action.to]
      if (to === undefined) return state
      // A subject already open in the target group is focused there instead of duplicated.
      const removed = withGroup({
        state,
        id: action.from,
        group: removeEditor({ group: from, key: action.key }),
      })
      const moved = withGroup({
        state: removed,
        id: action.to,
        group: insertEditor({ group: to, entry, index: action.index }),
      })
      return pruneGroup({ state: { ...moved, focusedGroup: action.to }, id: action.from })
    }
    case 'MoveToSplit': {
      const from = state.groups[action.from]
      const entry = from?.editors.find((e) => subjectKey(e.input) === action.key)
      if (from === undefined || entry === undefined) return state
      // Dragging a group's only editor beside itself would leave an empty group behind.
      if (action.from === action.target && from.editors.length === 1) return state
      const [withNew, fresh] = newGroup(state)
      const after = action.zone === 'right' || action.zone === 'bottom'
      const split = {
        ...withNew,
        editorArea: insertBeside({
          node: withNew.editorArea,
          target: action.target,
          fresh,
          dir: action.zone === 'left' || action.zone === 'right' ? 'row' : 'col',
          after,
        }),
        focusedGroup: fresh,
      }
      const removed = withGroup({
        state: split,
        id: action.from,
        group: removeEditor({ group: from, key: action.key }),
      })
      return pruneGroup({
        state: withGroup({
          state: removed,
          id: fresh,
          group: { editors: [entry], active: action.key },
        }),
        id: action.from,
      })
    }
    case 'Split': {
      // VS Code semantics: the active editor is duplicated into a new group beside the focused one.
      const group = state.groups[state.focusedGroup]
      const active = group?.editors.find((e) => subjectKey(e.input) === group.active)
      const [withNew, fresh] = newGroup(state)
      const split = {
        ...withNew,
        editorArea: insertBeside({
          node: withNew.editorArea,
          target: state.focusedGroup,
          fresh,
          dir: action.dir,
          after: true,
        }),
        focusedGroup: fresh,
      }
      return active === undefined
        ? split
        : withGroup({
            state: split,
            id: fresh,
            group: { editors: [active], active: subjectKey(active.input) },
          })
    }
    case 'Resize':
      return {
        ...state,
        editorArea: setSizesAt({ node: state.editorArea, path: action.path, sizes: action.sizes }),
      }
    case 'FocusGroup':
      return state.groups[action.group] === undefined
        ? state
        : { ...state, focusedGroup: action.group }
    case 'FocusGroupAt': {
      const id = groupOrder(state.editorArea)[action.index]
      return id === undefined ? state : { ...state, focusedGroup: id }
    }
    case 'Cycle': {
      const group = state.groups[state.focusedGroup]
      if (group === undefined || group.editors.length === 0) return state
      const index = group.editors.findIndex((e) => subjectKey(e.input) === group.active)
      const next =
        group.editors[(index + action.delta + group.editors.length) % group.editors.length]
      return next === undefined
        ? state
        : withGroup({
            state,
            id: state.focusedGroup,
            group: { ...group, active: subjectKey(next.input) },
          })
    }
    case 'MoveActive': {
      const group = state.groups[state.focusedGroup]
      const index = group?.editors.findIndex((e) => subjectKey(e.input) === group.active) ?? -1
      if (group === undefined || group.active === null || index === -1) return state
      return reduceLayout({
        state,
        action: {
          _tag: 'Move',
          from: state.focusedGroup,
          key: group.active,
          to: state.focusedGroup,
          index: index + action.delta,
        },
      })
    }
    case 'ToggleDock': {
      const dock = state.docks[action.dock]
      return {
        ...state,
        docks: { ...state.docks, [action.dock]: { ...dock, visible: !dock.visible } },
      }
    }
    case 'ShowView': {
      const dock = state.docks[action.dock]
      const next: DockState =
        action.view === null
          ? { ...dock, visible: false }
          : { ...dock, visible: true, activeView: action.view }
      return { ...state, docks: { ...state.docks, [action.dock]: next } }
    }
    case 'ResizeDock': {
      const dock = state.docks[action.dock]
      return { ...state, docks: { ...state.docks, [action.dock]: { ...dock, size: action.size } } }
    }
  }
}

const LayoutJsonString = Schema.fromJsonString(LayoutJson)
/** Serializes a layout for persistence. */
export const encodeLayout = Schema.encodeSync(LayoutJsonString)

/** Canonical URL: detail has no query; alternate presentations are explicit. */
export const subjectUrl = (address: SubjectAddress): string =>
  `/${address.ref.split('/').map(encodeURIComponent).join('/')}${address.presentation === 'detail' ? '' : `?presentation=${address.presentation}`}`

/** Untrusted links pass through the generated subject-id boundary before becoming navigation. */
export const parseSubjectUrl = (value: string): SubjectAddress | undefined => {
  try {
    const url = new URL(value, 'http://wf.local')
    const input = {
      ref: decodeURIComponent(url.pathname.slice(1)),
      presentation: url.searchParams.get('presentation') ?? 'detail',
    }
    const decoded = Schema.decodeUnknownExit(SubjectAddress)(input)
    return decoded._tag === 'Success' ? decoded.value : undefined
  } catch {
    return undefined
  }
}

/** Projects the active editor of the focused group into a subject URL. */
export const focusedSubjectUrl = ({ state }: { readonly state: WorkbenchState }): string | null => {
  const group = state.groups[state.focusedGroup]
  const entry = group?.editors.find((candidate) => subjectKey(candidate.input) === group.active)
  return entry === undefined ? null : subjectUrl(entry.input)
}

/** One empty group with the given docks. */
export const initialState = (docks: WorkbenchState['docks']): WorkbenchState => ({
  version: 2,
  editorArea: { _tag: 'group', id: 'g0' },
  groups: { g0: { editors: [], active: null } },
  focusedGroup: 'g0',
  docks,
  nextGroup: 1,
})

// ── Split-tree helpers ───────────────────────────────────────────────────────────────────────

const normalize = (sizes: ReadonlyArray<number>): number[] => {
  const total = sizes.reduce((sum, size) => sum + size, 0)
  return total <= 0 ? sizes.map(() => 1 / sizes.length) : sizes.map((size) => size / total)
}

/** Removes a group leaf; a split left with one child is replaced by that child. */
const removeLeaf = ({
  node,
  id,
}: {
  readonly node: SplitNode
  readonly id: GroupId
}): SplitNode | null => {
  if (node._tag === 'group') return node.id === id ? null : node
  const kept = node.children.flatMap((child, index) => {
    const next = removeLeaf({ node: child, id })
    return next === null ? [] : [{ child: next, size: node.sizes[index] ?? 0 }]
  })
  if (kept.length === 0) return null
  if (kept.length === 1) return kept[0]?.child ?? null
  return { ...node, children: kept.map((k) => k.child), sizes: normalize(kept.map((k) => k.size)) }
}

/** Puts `fresh` beside leaf `target`: joins the parent split when the direction matches, else nests. */
const insertBeside = ({
  node,
  target,
  fresh,
  dir,
  after,
}: {
  readonly node: SplitNode
  readonly target: GroupId
  readonly fresh: GroupId
  readonly dir: 'row' | 'col'
  readonly after: boolean
}): SplitNode => {
  const leaf: GroupLeaf = { _tag: 'group', id: fresh }
  if (node._tag === 'group') {
    if (node.id !== target) return node
    return { _tag: 'split', dir, children: after ? [node, leaf] : [leaf, node], sizes: [0.5, 0.5] }
  }
  const index = node.children.findIndex((child) => child._tag === 'group' && child.id === target)
  if (index !== -1 && node.dir === dir) {
    const half = (node.sizes[index] ?? 0) / 2
    const at = after ? index + 1 : index
    return {
      ...node,
      children: [...node.children.slice(0, at), leaf, ...node.children.slice(at)],
      sizes: [...node.sizes.slice(0, index), half, half, ...node.sizes.slice(index + 1)],
    }
  }
  return {
    ...node,
    children: node.children.map((child) =>
      insertBeside({ node: child, target, fresh, dir, after }),
    ),
  }
}

const setSizesAt = ({
  node,
  path,
  sizes,
}: {
  readonly node: SplitNode
  readonly path: ReadonlyArray<number>
  readonly sizes: ReadonlyArray<number>
}): SplitNode => {
  if (node._tag === 'group') return node
  const [head, ...rest] = path
  if (head === undefined)
    return node.sizes.length === sizes.length ? { ...node, sizes: normalize(sizes) } : node
  return {
    ...node,
    children: node.children.map((child, i) =>
      i === head ? setSizesAt({ node: child, path: rest, sizes }) : child,
    ),
  }
}

// ── Group helpers ────────────────────────────────────────────────────────────────────────────

const withGroup = ({
  state,
  id,
  group,
}: {
  readonly state: WorkbenchState
  readonly id: GroupId
  readonly group: Group
}): WorkbenchState => ({
  ...state,
  groups: { ...state.groups, [id]: group },
})

const newGroup = (state: WorkbenchState): readonly [WorkbenchState, GroupId] => {
  const id = `g${state.nextGroup}`
  return [
    {
      ...withGroup({ state, id, group: { editors: [], active: null } }),
      nextGroup: state.nextGroup + 1,
    },
    id,
  ]
}

/** Drops an empty group unless it is the last one; focus moves to the first remaining group. */
const pruneGroup = ({
  state,
  id,
}: {
  readonly state: WorkbenchState
  readonly id: GroupId
}): WorkbenchState => {
  const group = state.groups[id]
  if (group === undefined || group.editors.length > 0) return state
  const layout = removeLeaf({ node: state.editorArea, id })
  if (layout === null) return state
  const { [id]: _removed, ...groups } = state.groups
  const order = groupOrder(layout)
  return {
    ...state,
    editorArea: layout,
    groups,
    focusedGroup: state.focusedGroup === id ? (order[0] ?? state.focusedGroup) : state.focusedGroup,
  }
}

const removeEditor = ({ group, key }: { readonly group: Group; readonly key: string }): Group => {
  const index = group.editors.findIndex((entry) => subjectKey(entry.input) === key)
  if (index === -1) return group
  const editors = group.editors.filter((_, i) => i !== index)
  const neighbour = editors[Math.min(index, editors.length - 1)]
  return {
    editors,
    active:
      group.active === key
        ? neighbour === undefined
          ? null
          : subjectKey(neighbour.input)
        : group.active,
  }
}

const insertEditor = ({
  group,
  entry,
  index,
}: {
  readonly group: Group
  readonly entry: EditorEntry
  readonly index: number
}): Group => {
  const key = subjectKey(entry.input)
  const editors = group.editors.filter((e) => subjectKey(e.input) !== key)
  const at = Math.max(0, Math.min(index, editors.length))
  return { editors: [...editors.slice(0, at), entry, ...editors.slice(at)], active: key }
}

/** Opens an ordinary surface: focus an existing one or insert after the active surface. */
const openIn = ({
  state,
  groupId,
  input,
}: {
  readonly state: WorkbenchState
  readonly groupId: GroupId
  readonly input: SubjectAddress
}): WorkbenchState => {
  const group = state.groups[groupId] ?? { editors: [], active: null }
  const key = subjectKey(input)
  if (group.editors.some((entry) => subjectKey(entry.input) === key)) {
    if (group.active === key && state.focusedGroup === groupId) return state
    return {
      ...withGroup({ state, id: groupId, group: { ...group, active: key } }),
      focusedGroup: groupId,
    }
  }
  const activeIndex = group.editors.findIndex((entry) => subjectKey(entry.input) === group.active)
  const editors = [
    ...group.editors.slice(0, activeIndex + 1),
    { input },
    ...group.editors.slice(activeIndex + 1),
  ]
  return {
    ...withGroup({ state, id: groupId, group: { editors, active: key } }),
    focusedGroup: groupId,
  }
}
