// Controlled presentation/model only.
/**
 * Workbench layout model, aligned with the shared workspace resource: a
 * binary tree of splits whose leaves are tab groups. Pane keys use the
 * shared grammar `<uri>[ form=…][ view=…]` and stay opaque to the layout.
 */

import type { ConversationRuntimeOptions } from '../EmbraceRuntime'
import type { TranscriptProps } from '../composition/Transcript'
import type { DiffFile } from '../composition/DiffPanel'
import type { TerminalFrame } from './terminal-model'


/** Diff payload for a `diff:` pane. */
export interface DiffResource {
  readonly lines: readonly string[]
  readonly path: string
  readonly added: number
  readonly removed: number
  readonly branchFiles?: readonly DiffFile[]
  readonly currentTurnFiles?: readonly DiffFile[]
}

/** Exact host snapshots: turn boundaries, lifecycle, work, sync and clock are never inferred here. */
export interface ThreadResource {
  readonly title?: string
  readonly runtime: ConversationRuntimeOptions
  readonly transcript: Omit<TranscriptProps, 'title' | 'viewportKey'>
}

export interface ThreadControls {
  readonly folder: string
  readonly branch: string
  readonly onSend?: (text: string) => void | Promise<void>
  readonly onSteer?: (text: string) => void | Promise<void>
  readonly onStop?: () => void | Promise<void>
}

/** Per-uri resources the pane host renders; supplied by the app or a fixture. */
export interface WorkbenchResources {
  readonly threads: ReadonlyMap<string, ThreadResource>
  readonly threadControls?: ReadonlyMap<string, ThreadControls>
  readonly diffs: ReadonlyMap<string, DiffResource>
  readonly terminals: TerminalFrame
}
/** One open resource: `uri` plus optional `form` (rendering mode) and `view` (device-local view state id). */
export interface WorkbenchPane {
  readonly uri: string
  readonly form?: string
  readonly view?: string
}

/** Canonical pane key: `<uri>[ form=…][ view=…]`. */
export const paneKey = (pane: WorkbenchPane): string =>
  [pane.uri, pane.form === undefined ? undefined : `form=${pane.form}`, pane.view === undefined ? undefined : `view=${pane.view}`]
    .filter((part): part is string => part !== undefined)
    .join(' ')

/** Inverse of {@link paneKey}; unknown segments are ignored. */
export const parsePaneKey = (key: string): WorkbenchPane => {
  const parts = key.split(' ')
  const uri = parts[0] ?? ''
  let form: string | undefined
  let view: string | undefined
  for (const part of parts.slice(1)) {
    if (part.startsWith('form=')) form = part.slice('form='.length)
    else if (part.startsWith('view=')) view = part.slice('view='.length)
  }
  return form === undefined && view === undefined ? { uri } : { uri, form, view }
}

/** Terminal sessions are drawer resources, not split-tree panes. */
export type PaneKind = 'thread' | 'diff' | 'placeholder'

/** Resource kind derived from the uri scheme; unknown schemes render as placeholders. */
export const paneKind = (pane: WorkbenchPane): PaneKind => {
  if (pane.uri.startsWith('agent:') || pane.uri.startsWith('agent/')) return 'thread'
  if (pane.uri.startsWith('diff:')) return 'diff'
  return 'placeholder'
}

/** Short tab title: the uri tail after its scheme colon. */
export const paneTitle = (pane: WorkbenchPane): string => {
  const tail = pane.uri.includes(':') ? pane.uri.slice(pane.uri.indexOf(':') + 1) : pane.uri
  const last = tail.split('/').filter(Boolean).at(-1) ?? tail
  return last === '' ? pane.uri : last
}

/** Leaf: a tab group of panes; the first tab is shown. */
export interface TabGroupLayout {
  readonly kind: 'group'
  readonly tabs: readonly WorkbenchPane[]
}

/** Binary split: `split` is the axis the second child is placed on; `ratio` is the first child's share. */
export interface SplitLayout {
  readonly kind: 'split'
  readonly split: 'right' | 'below'
  readonly ratio?: number
  readonly children: readonly [WorkbenchLayout, WorkbenchLayout]
}

export type WorkbenchLayout = TabGroupLayout | SplitLayout

export const group = (tabs: readonly WorkbenchPane[]): TabGroupLayout => ({ kind: 'group', tabs })

/** Exact pane selection wins over a stored selection; stale storage falls back to the first tab. */
export const displayedTabKey = (keys: readonly string[], focusedPaneKey: string | undefined, storedActive: string | null): string =>
  focusedPaneKey !== undefined && keys.includes(focusedPaneKey) ? focusedPaneKey : storedActive !== null && keys.includes(storedActive) ? storedActive : keys[0]!

export const isFocusedPaneDisplayed = (layout: WorkbenchLayout, focusedPaneKey: string): boolean =>
  layout.kind === 'group'
    ? displayedTabKey(layout.tabs.map(paneKey), focusedPaneKey, null) === focusedPaneKey
    : isFocusedPaneDisplayed(layout.children[0], focusedPaneKey) || isFocusedPaneDisplayed(layout.children[1], focusedPaneKey)

export const split = (
  axis: 'right' | 'below',
  first: WorkbenchLayout,
  second: WorkbenchLayout,
  ratio = 0.5,
): SplitLayout => ({ kind: 'split', split: axis, ratio, children: [first, second] })
/**
 * Index path of a node from the root: the root is "0" and each descent
 * appends ".0"/".1". The leading "0" names the root itself — it is never a
 * child index — and is stripped once, here, for every walker.
 */
export type LayoutPath = string

type Segment = 0 | 1

const parsePath = (path: LayoutPath): readonly Segment[] | null => {
  const parts = path.split('.')
  if (parts[0] !== '0') return null
  const segments: Segment[] = []
  for (const part of parts.slice(1)) {
    if (part !== '0' && part !== '1') return null
    segments.push(part === '0' ? 0 : 1)
  }
  return segments
}

const childPath = (path: LayoutPath, index: Segment): LayoutPath => `${path}.${index}`

const replaceSegments = (layout: WorkbenchLayout, segments: readonly Segment[], next: WorkbenchLayout): WorkbenchLayout => {
  if (segments.length === 0) return next
  if (layout.kind !== 'split') return layout
  const index = segments[0]!
  const child = layout.children[index]
  const replaced = replaceSegments(child, segments.slice(1), next)
  if (replaced === child) return layout
  const children: readonly [WorkbenchLayout, WorkbenchLayout] = index === 0 ? [replaced, layout.children[1]] : [layout.children[0], replaced]
  return { ...layout, children }
}

/** Replaces the node at `path` (root is "0"); returns the same reference when nothing changes. */
export const replaceAtPath = (layout: WorkbenchLayout, path: LayoutPath, next: WorkbenchLayout): WorkbenchLayout => {
  const segments = parsePath(path)
  if (segments === null) return layout
  return replaceSegments(layout, segments, next)
}

const nodeAtSegments = (layout: WorkbenchLayout, segments: readonly Segment[]): WorkbenchLayout | null => {
  let node: WorkbenchLayout = layout
  for (const index of segments) {
    if (node.kind !== 'split') return null
    node = node.children[index]
  }
  return node
}

export const nodeAtPath = (layout: WorkbenchLayout, path: LayoutPath): WorkbenchLayout | null => {
  const segments = parsePath(path)
  return segments === null ? null : nodeAtSegments(layout, segments)
}

/** Closing a sole-tab group promotes its sibling without rebuilding the surviving subtree. */
export const removeGroupAtPath = (layout: WorkbenchLayout, path: LayoutPath): WorkbenchLayout => {
  const segments = parsePath(path)
  if (segments === null) return layout
  if (segments.length === 0) return group([{ uri: 'about:blank' }])
  const parentSegments = segments.slice(0, -1)
  const parent = nodeAtSegments(layout, parentSegments)
  if (parent?.kind !== 'split') return layout
  return replaceSegments(layout, parentSegments, parent.children[segments[segments.length - 1] === 0 ? 1 : 0])
}

/** Adds a pane to the tab group at `path`. */
export const addTabAtPath = (layout: WorkbenchLayout, path: LayoutPath, pane: WorkbenchPane): WorkbenchLayout => {
  const node = nodeAtPath(layout, path)
  if (node === null || node.kind !== 'group') return layout
  if (node.tabs.some(tab => paneKey(tab) === paneKey(pane))) return layout
  return replaceAtPath(layout, path, group([...node.tabs, pane]))
}

/**
 * Replaces the tab group at `path` with a split keeping the existing group as
 * the first child and a fresh group holding `pane` as the second.
 */
export const splitGroupAtPath = (
  layout: WorkbenchLayout,
  path: LayoutPath,
  axis: 'right' | 'below',
  pane: WorkbenchPane,
): WorkbenchLayout => {
  const node = nodeAtPath(layout, path)
  if (node === null || node.kind !== 'group') return layout
  return replaceAtPath(layout, path, split(axis, node, group([pane])))
}

/** Finds a group by exact pane key or resource URI, or the first group for a new pane. */
export const findGroupPath = (layout: WorkbenchLayout, key?: string, path: LayoutPath = '0'): LayoutPath | undefined => {
  if (layout.kind === 'group') return key === undefined || layout.tabs.some(pane => paneKey(pane) === key || pane.uri === key) ? path : undefined
  return findGroupPath(layout.children[0], key, childPath(path, 0)) ?? findGroupPath(layout.children[1], key, childPath(path, 1))
}

/** All pane keys, including inactive tabs, for the surface's scroll-memory retention boundary. */
export const layoutPaneKeys = (layout: WorkbenchLayout): readonly string[] =>
  layout.kind === 'group' ? layout.tabs.map(paneKey) : [...layoutPaneKeys(layout.children[0]), ...layoutPaneKeys(layout.children[1])]

/** Every split path with its axis and stored ratio default, depth-first. */
export const walkSplits = (layout: WorkbenchLayout, path: LayoutPath = '0'): readonly { readonly path: LayoutPath; readonly node: SplitLayout }[] => {
  if (layout.kind !== 'split') return []
  return [
    { path, node: layout },
    ...walkSplits(layout.children[0], childPath(path, 0)),
    ...walkSplits(layout.children[1], childPath(path, 1)),
  ]
}
