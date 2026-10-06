import { generateNKeysBetween } from 'fractional-indexing'
import type { ArrangementOperation } from '@smalltalk/st3-client'
import { compareString, drawnParents, isLive, liveAncestor } from './legacy.ts'
import type { FolderDoc } from './legacy.ts'

export class MigrationInputError extends TypeError {
  constructor(readonly code: 'invalid-name' | 'invalid-folder-id' | 'partial-folder' | 'atomic-import-limit', message: string) { super(message) }
}
const uuid = /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/
const requireName = (name: string): void => {
  if (!name.trim() || /\p{Cc}/u.test(name) || new TextEncoder().encode(name).byteLength > 256)
    throw new MigrationInputError('invalid-name', 'Legacy name cannot be admitted unchanged; source remains staged and unmodified')
}

/** Preserve stable IDs, projected ordering, raw known placements and remove-wins deletion.
 * HLC stamps stay in the staged source, never become new live ordering authority. */
export const importOperations = (doc: FolderDoc, name: string): ArrangementOperation[] => {
  requireName(name)
  const effective = drawnParents(doc)
  const parents = new Map<string, string | null>()
  const groups = new Map<string | null, string[]>()
  for (const [id, folder] of Object.entries(doc.folders)) {
    if (!uuid.test(id)) throw new MigrationInputError('invalid-folder-id', `Legacy folder ${id} is not a lowercase UUIDv7`)
    if (folder.deleted === undefined && (!isLive(doc, id) || folder.name === undefined))
      throw new MigrationInputError('partial-folder', `Legacy folder ${id} is incomplete; refusing to resurrect or discard it`)
    if (folder.deleted === undefined) requireName(folder.name!.value)
    // Deleted positions are retained through their nearest live ancestor. Live cycles
    // are resolved by the old reducer before new local admission (which refuses cycles).
    const parent = folder.deleted === undefined ? effective.get(id) ?? null : liveAncestor(doc, folder.position?.parent ?? null)
    parents.set(id, parent)
    const siblings = groups.get(parent) ?? []
    siblings.push(id)
    groups.set(parent, siblings)
  }
  const keys = new Map<string, string>()
  for (const siblings of groups.values()) {
    siblings.sort((left, right) => compareString(doc.folders[left]?.position?.key ?? '', doc.folders[right]?.position?.key ?? '') || compareString(left, right))
    const canonical = generateNKeysBetween(null, null, siblings.length)
    siblings.forEach((id, index) => { const key = canonical[index]; if (key === undefined) throw new RangeError('Missing canonical folder key'); keys.set(id, key) })
  }
  const operations: ArrangementOperation[] = [{ op: 'create', name }]
  const created = new Set<string>()
  const create = (id: string): void => {
    if (created.has(id)) return
    const parent = parents.get(id) ?? null
    if (parent !== null) create(parent)
    const folder = doc.folders[id]
    const key = keys.get(id)
    if (folder === undefined || key === undefined) throw new RangeError('Missing planned folder')
    operations.push({ op: 'folder.create', id, name: folder.deleted === undefined ? folder.name!.value : 'Deleted folder', parent, key })
    created.add(id)
  }
  for (const id of Object.keys(doc.folders).sort(compareString)) create(id)
  const members = new Map<string | null, string[]>()
  for (const [subject, placement] of Object.entries(doc.placements)) {
    const effectiveFolder = liveAncestor(doc, placement.folder)
    const list = members.get(effectiveFolder) ?? []
    list.push(subject)
    members.set(effectiveFolder, list)
  }
  for (const [, subjects] of [...members].sort(([left], [right]) => compareString(left ?? '', right ?? ''))) {
    subjects.sort((left, right) => compareString(doc.placements[left]!.key, doc.placements[right]!.key) || compareString(left, right))
    const canonical = generateNKeysBetween(null, null, subjects.length)
    subjects.forEach((subject, index) => {
      const placement = doc.placements[subject]
      const key = canonical[index]
      if (placement === undefined || key === undefined) throw new RangeError('Missing planned placement')
      operations.push({ op: 'subject.place', subject, folder: placement.folder !== null && Object.hasOwn(doc.folders, placement.folder) ? placement.folder : null, key })
    })
  }
  // Place first, then delete: normal admission refuses edits targeting an already-deleted folder.
  for (const id of Object.keys(doc.folders).sort(compareString)) if (doc.folders[id]?.deleted !== undefined)
    operations.push({ op: 'folder.delete', id })
  if (operations.length > 1024 || new TextEncoder().encode(JSON.stringify(operations)).byteLength > 1024 * 1024)
    throw new MigrationInputError('atomic-import-limit', 'Import exceeds one atomic arrangements edit; refusing a partial, concurrently editable target')
  return operations
}
