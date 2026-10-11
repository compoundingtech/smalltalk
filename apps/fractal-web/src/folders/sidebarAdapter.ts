import { generateKeyBetween, generateNKeysBetween } from 'fractional-indexing'
import { drawnParents, insertAt, isLive, liveAncestor, positionKey, project, wouldCycle, type FolderDoc, type Insertion, type Projection, type ProjectedFolder } from './core.mts'
import { refusalText, sidebarOperations, type ArrangementEditorState, type RestageVerdict, type SidebarOperation } from './edit.ts'
import type { SidebarCreateFolder, SidebarDropVerdict, SidebarMove, SidebarNode, SidebarRowStatus } from './sidebarContract.ts'

/** A seat's row key is not its shared membership subject. Lifecycle does not filter this roster. */
export interface SidebarSeat { readonly id: string; readonly subject: string; readonly host: string; readonly label: string }
export type SidebarDocument = FolderDoc & { readonly roster: readonly SidebarSeat[] }
export type SidebarCollapse = ReadonlyMap<string, boolean>
const compare = (left: string, right: string): number => {
  const a = Array.from(left, (value) => value.codePointAt(0)!)
  const b = Array.from(right, (value) => value.codePointAt(0)!)
  for (let i = 0; i < Math.min(a.length, b.length); i++) if (a[i] !== b[i]) return a[i]! - b[i]!
  return a.length - b.length
}

export const sidebarTree = (projection: Projection, roster: readonly SidebarSeat[], collapse: SidebarCollapse): SidebarNode[] => {
  const seats = new Map<string, SidebarNode[]>()
  for (const seat of roster) {
    const rows = seats.get(seat.subject) ?? []
    rows.push({ _tag: 'Agent', id: seat.id, subject: seat.subject, label: seat.label })
    seats.set(seat.subject, rows)
  }
  const folder = (item: ProjectedFolder): SidebarNode => ({
    _tag: 'Folder', id: item.id, label: item.name, collapsed: collapse.get(item.id) ?? false,
    children: [...item.folders.map(folder), ...item.members.flatMap((subject) => seats.get(subject) ?? [])],
  })
  const unfiled = new Set(projection.unfiled)
  const rows = roster.filter(seat => unfiled.has(seat.subject)).toSorted((a, b) => compare(a.host, b.host) || compare(a.id, b.id))
  return [...projection.folders.map(folder), ...(rows.length === 0 ? [] : [{
    _tag: 'Group' as const, id: 'wf/unfiled', label: 'Unfiled', collapsed: collapse.get('wf/unfiled') ?? false,
    children: rows.map((seat): SidebarNode => ({ _tag: 'Agent', id: seat.id, subject: seat.subject, label: seat.label })),
  }])]
}

/** Filtering is a view operation; all move plans continue to use the complete document. */
export const filterSidebarTree = (tree: readonly SidebarNode[], query: string): SidebarNode[] => {
  const needle = query.trim().toLocaleLowerCase()
  if (needle === '') return [...tree]
  return tree.flatMap((node): SidebarNode[] => {
    if (node._tag === 'Agent') return node.label.toLocaleLowerCase().includes(needle) ? [node] : []
    const children = filterSidebarTree(node.children, query)
    return children.length === 0 ? [] : [{ ...node, collapsed: false, children }]
  })
}

type Item = { readonly kind: 'folder' | 'agent'; readonly id: string; readonly parent: string | null }
const itemFor = (doc: SidebarDocument, id: string): Item | undefined => {
  if (isLive(doc, id)) return { kind: 'folder', id, parent: drawnParents(doc).get(id) ?? null }
  const seat = doc.roster.find((row) => row.id === id)
  return seat === undefined ? undefined : { kind: 'agent', id: seat.subject, parent: liveAncestor(doc, doc.placements[seat.subject]?.folder ?? null) }
}
type Sibling = { readonly id: string; readonly key: string; readonly visible: boolean }
/** The full stored partition: dormant placements (absent from the roster) keep reserved keys but no visible position. */
const siblingsFor = (doc: FolderDoc & { readonly roster?: readonly SidebarSeat[] }, kind: Item['kind'], parent: string | null): Sibling[] => {
  if (kind === 'folder') return [...drawnParents(doc)].filter(([, value]) => value === parent).map(([id]) => ({ id, key: positionKey(doc, id), visible: true })).sort((a, b) => compare(a.key, b.key) || compare(a.id, b.id))
  const live = doc.roster === undefined ? undefined : new Set(doc.roster.map((seat) => seat.subject))
  return Object.entries(doc.placements).filter(([, value]) => liveAncestor(doc, value.folder) === parent)
    .map(([id, value]) => ({ id, key: value.key, visible: live === undefined || live.has(id) })).sort((a, b) => compare(a.key, b.key) || compare(a.id, b.id))
}

/** `siblings` is the full partition without the item; `index` is the key slot, already mapped past dormant keys. */
type MovePlan = { readonly item: Item; readonly parent: string | null; readonly index: number; readonly siblings: readonly Sibling[]; readonly unchanged: boolean }
const planMove = (doc: SidebarDocument, move: SidebarMove): MovePlan | { refused: string } => {
  if (move.items.length !== 1) return { refused: 'Move one item at a time.' }
  const item = itemFor(doc, move.items[0])
  if (item === undefined) return { refused: 'This item is no longer available.' }
  const target = move.target
  let parent: string | null
  let sibling: Item | undefined
  if (target._tag === 'Into') {
    if (!isLive(doc, target.folder)) return { refused: 'This folder is no longer available.' }
    parent = target.folder
  } else if (target._tag === 'Root') {
    if (item.kind !== 'folder') return { refused: 'Unfiled agents use automatic host order.' }
    if (!Number.isInteger(target.index) || target.index < 0) return { refused: 'Choose a valid folder position.' }
    parent = null
  } else if (target._tag === 'Unfiled') {
    if (item.kind !== 'agent') return { refused: 'Folders cannot move into Unfiled.' }
    parent = null
  } else {
    sibling = itemFor(doc, target.sibling)
    if (sibling === undefined) return { refused: 'This item is no longer available.' }
    if (sibling.kind !== item.kind) return { refused: 'Folders and agents have separate ordering.' }
    parent = sibling.parent
    if (item.kind === 'agent' && parent === null) return { refused: 'Unfiled agents use automatic host order.' }
    // A gap inside the consecutive seats of one subject is never a membership slot.
    if (item.kind === 'agent') {
      const rows = doc.roster.filter((seat) => seat.subject === sibling?.id)
      const edge = target._tag === 'Before' ? rows[0] : rows.at(-1)
      if (edge?.id !== target.sibling) return { refused: 'Move between agents, not between seats of one agent.' }
    }
  }
  if (item.kind === 'folder' && wouldCycle(doc, item.id, parent)) return { refused: 'A folder cannot go inside itself or its subfolders.' }
  const all = siblingsFor(doc, item.kind, parent)
  const visible = all.filter((row) => row.visible)
  const others = visible.filter((row) => row.id !== item.id)
  let index = others.length
  if (target._tag === 'Root') {
    if (target.index > visible.length) return { refused: 'Choose a valid folder position.' }
    // Root targets name the gap in the current tree, before removal of an existing root mover.
    const from = visible.findIndex((row) => row.id === item.id)
    index = target.index - (from >= 0 && from < target.index ? 1 : 0)
  } else if (sibling !== undefined && sibling.id !== item.id) {
    index = others.findIndex((row) => row.id === sibling.id) + (target._tag === 'After' ? 1 : 0)
  }
  const unchanged = sibling?.id === item.id || (item.parent === parent && (parent === null && item.kind === 'agent' || visible.findIndex((row) => row.id === item.id) === index))
  const siblings = all.filter((row) => row.id !== item.id)
  const anchor = others[index]
  return { item, parent, siblings, index: anchor === undefined ? siblings.length : siblings.indexOf(anchor), unchanged }
}

/**
 * st admits only canonical fractional-indexing keys (generated ArrangementKey), not core.mts's legacy claim alphabet.
 * Invalid or colliding neighbour keys are rekeyed in the same edit; if no local gap exists, the whole partition is rekeyed.
 */
export const nativeInsertAt = (siblings: readonly string[], index: number): Insertion => {
  try { return insertAt(siblings, index, generateKeyBetween) } catch {
    const keys = generateNKeysBetween(null, null, siblings.length + 1)
    return { key: keys[index]!, rekeyed: siblings.map((_, at): [number, string] => [at, keys[at < index ? at : at + 1]!]) }
  }
}
export const canDrop = (doc: SidebarDocument, move: SidebarMove): SidebarDropVerdict => {
  const plan = planMove(doc, move)
  return 'refused' in plan ? plan : { ok: true }
}
/** Fresh-state verdict: an invalid plan stays refused with fixed copy; an already-satisfied plan writes nothing. */
export const restageMove = (doc: SidebarDocument, move: SidebarMove): RestageVerdict => {
  const plan = planMove(doc, move)
  if ('refused' in plan) return { _tag: 'Refused', sentence: plan.refused }
  if (plan.unchanged) return { _tag: 'Satisfied' }
  if (plan.item.kind === 'agent' && plan.parent === null) return { _tag: 'Operations', operations: [sidebarOperations.placeSubject({ subject: plan.item.id, folder: null, key: 'a0' })] }
  const insertion = nativeInsertAt(plan.siblings.map((row) => row.key), plan.index)
  const operation = (id: string, key: string): SidebarOperation => plan.item.kind === 'folder'
    ? sidebarOperations.moveFolder({ id, parent: plan.parent, key })
    : sidebarOperations.placeSubject({ subject: id, folder: plan.parent, key })
  return { _tag: 'Operations', operations: [...insertion.rekeyed.map(([index, key]) => operation(plan.siblings[index]!.id, key)), operation(plan.item.id, insertion.key)] }
}
export const operationsForMove = (doc: SidebarDocument, move: SidebarMove): SidebarOperation[] => {
  const verdict = restageMove(doc, move)
  return verdict._tag === 'Operations' ? [...verdict.operations] : []
}
/** Agent rows a move names, bound to the subjects they showed when the gesture was accepted. */
export const boundRows = (roster: readonly SidebarSeat[], move: SidebarMove): ReadonlyMap<string, string> => {
  const rows = [move.items[0], ...(move.target._tag === 'Before' || move.target._tag === 'After' ? [move.target.sibling] : [])]
  return new Map(rows.flatMap((id) => {
    const seat = roster.find((row) => row.id === id)
    return seat === undefined ? [] : [[id, seat.subject] as const]
  }))
}
/** A row key rebound to another subject must never redirect a retried move to that subject. */
export const restageBoundMove = (doc: SidebarDocument, move: SidebarMove, bound: ReadonlyMap<string, string>): RestageVerdict => {
  for (const [id, subject] of bound) if (doc.roster.find((row) => row.id === id)?.subject !== subject)
    return { _tag: 'Refused', sentence: 'This agent changed; select it again.' }
  return restageMove(doc, move)
}
export const operationsForCreate = (doc: FolderDoc, intent: SidebarCreateFolder, id: string): SidebarOperation[] => {
  const parent = intent.withAgent !== undefined ? null : intent.parent !== null && isLive(doc, intent.parent) ? intent.parent : null
  const siblings = siblingsFor(doc, 'folder', parent)
  const insertion = nativeInsertAt(siblings.map((row) => row.key), siblings.length)
  return [...insertion.rekeyed.map(([index, key]) => sidebarOperations.moveFolder({ id: siblings[index]!.id, parent, key })),
    sidebarOperations.createFolder({ id, parent, key: insertion.key, name: intent.name.trim() || 'New folder' }),
    ...(intent.withAgent === undefined ? [] : [sidebarOperations.placeSubject({ subject: intent.withAgent, folder: id, key: 'a0' })])]
}
export const restageCreate = (doc: FolderDoc, intent: SidebarCreateFolder, id: string): RestageVerdict =>
  isLive(doc, id) ? { _tag: 'Satisfied' }
  : doc.folders[id] !== undefined ? { _tag: 'Refused', sentence: 'This folder was deleted; create it again.' }
  : { _tag: 'Operations', operations: operationsForCreate(doc, intent, id) }
export const restageRename = (doc: FolderDoc, intent: { readonly id: string; readonly name: string }): RestageVerdict =>
  !isLive(doc, intent.id) ? { _tag: 'Refused', sentence: 'This folder is no longer available.' }
  : doc.folders[intent.id]?.name?.value === (intent.name.trim() || 'New folder') ? { _tag: 'Satisfied' }
  : { _tag: 'Operations', operations: operationsForRename(doc, intent) }
export const restageDelete = (doc: FolderDoc, intent: { readonly id: string }): RestageVerdict =>
  isLive(doc, intent.id) ? { _tag: 'Operations', operations: operationsForDelete(doc, intent) }
  : doc.folders[intent.id]?.deleted !== undefined ? { _tag: 'Satisfied' } : { _tag: 'Refused', sentence: 'This folder is no longer available.' }
export const operationsForRename = (doc: FolderDoc, intent: { readonly id: string; readonly name: string }): SidebarOperation[] => {
  const name = intent.name.trim() || 'New folder'
  return !isLive(doc, intent.id) || doc.folders[intent.id]?.name?.value === name ? [] : [sidebarOperations.renameFolder({ id: intent.id, name })]
}
/** Tombstoning alone lifts contents with their original keys; never rewrite or delete members. */
export const operationsForDelete = (doc: FolderDoc, intent: { readonly id: string }): SidebarOperation[] =>
  isLive(doc, intent.id) ? [sidebarOperations.deleteFolder({ id: intent.id })] : []

export const rowStatus = (editorState: Pick<ArrangementEditorState, 'phase' | 'refusal' | 'retryReady'> & { readonly onRetry?: () => void }, pendingTargets: ReadonlyMap<string, string>): Map<string, SidebarRowStatus> => {
  const result = new Map<string, SidebarRowStatus>()
  for (const [row, target] of pendingTargets) {
    if (editorState.refusal?.targets.includes(target)) result.set(row, {
      _tag: 'Refused', reason: refusalText(editorState.refusal),
      ...(editorState.retryReady && editorState.onRetry !== undefined ? { onRetry: editorState.onRetry } : {}),
    })
    else if (editorState.phase === 'pending') result.set(row, { _tag: 'Pending' })
  }
  return result
}

export const sidebarProjection = (doc: SidebarDocument): Projection => project(doc, new Set(doc.roster.map((seat) => seat.subject)))
