// Legacy sidebar claim decoder/fold. Delete after every fenced source site is imported.
export type Stamp = [milliseconds: number, counter: number, writer: string]
export type FolderId = string
export type AgentSubject = string
export interface NameReg { value: string; at: Stamp }
export interface PositionReg { parent: FolderId | null; key: string; at: Stamp }
export interface PlacementReg { folder: FolderId | null; key: string; at: Stamp }
export interface FolderRegs { name?: NameReg; position?: PositionReg; deleted?: Stamp }
export interface FolderDoc { folders: Record<FolderId, FolderRegs>; placements: Record<AgentSubject, PlacementReg> }
export const claimKind = 'custom.fractal.sidebar'
export const claimFormat = 1
const maxCounter = 0xffffffff
export class FolderValidationError extends TypeError {
  readonly path: string
  constructor(path: string, expected: string) {
    super(`${path}: expected ${expected}`)
    this.name = 'FolderValidationError'
    this.path = path
  }
}

const isObject = (value: unknown): value is Record<string, unknown> => value !== null && typeof value === 'object' && !Array.isArray(value)
const isArray = (value: unknown): value is unknown[] => Array.isArray(value)
const object = (value: unknown, path: string): Record<string, unknown> => {
  if (!isObject(value)) {
    throw new FolderValidationError(path, 'an object')
  }
  return value
}
const string = (value: unknown, path: string): string => {
  if (typeof value !== 'string' || !value.isWellFormed()) {
    throw new FolderValidationError(path, 'a Unicode string')
  }
  return value
}
const integer = (value: unknown, path: string, max = Number.MAX_SAFE_INTEGER): number => {
  // Rust uses u64. Reject values JavaScript cannot represent losslessly rather
  // than silently changing register order or an idempotency key.
  if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 0 || value > max) {
    throw new FolderValidationError(path, `an integer from 0 to ${max}`)
  }
  return value
}
const optionalId = (value: unknown, path: string): FolderId | null => value == null ? null : string(value, path)
const stamp = (value: unknown, path: string): Stamp => {
  if (!isArray(value) || value.length !== 3) {
    throw new FolderValidationError(path, '[milliseconds, counter, writer]')
  }
  return [integer(value[0], `${path}[0]`), integer(value[1], `${path}[1]`, maxCounter), string(value[2], `${path}[2]`)]
}
const nameReg = (value: unknown, path: string): NameReg => {
  const reg = object(value, path)
  return { value: string(reg.value, `${path}.value`), at: stamp(reg.at, `${path}.at`) }
}
const positionReg = (value: unknown, path: string): PositionReg => {
  const reg = object(value, path)
  return { parent: optionalId(reg.parent, `${path}.parent`), key: string(reg.key, `${path}.key`), at: stamp(reg.at, `${path}.at`) }
}
const placementReg = (value: unknown, path: string): PlacementReg => {
  const reg = object(value, path)
  return { folder: optionalId(reg.folder, `${path}.folder`), key: string(reg.key, `${path}.key`), at: stamp(reg.at, `${path}.at`) }
}
const folderRegs = (value: unknown, path: string): FolderRegs => {
  const regs = object(value, path)
  const result: FolderRegs = {}
  if (regs.name != null) result.name = nameReg(regs.name, `${path}.name`)
  if (regs.position != null) result.position = positionReg(regs.position, `${path}.position`)
  if (regs.deleted != null) result.deleted = stamp(regs.deleted, `${path}.deleted`)
  return result
}
const map = <TValue,>(value: unknown, path: string, decode: (value: unknown, path: string) => TValue): Record<string, TValue> => Object.fromEntries(
  Object.entries(object(value, path)).map(([key, item]) => [string(key, path), decode(item, `${path}[${JSON.stringify(key)}]`)]),
)

export const emptyDoc = (): FolderDoc => ({ folders: {}, placements: {} })
/** Decodes serde defaults/nullable registers, drops unknown fields, rejects malformed wire data. */
export const decodeDoc = (value: unknown): FolderDoc => {
  const doc = object(value, 'doc')
  return {
    folders: doc.folders === undefined ? {} : map(doc.folders, 'doc.folders', folderRegs),
    placements: doc.placements === undefined ? {} : map(doc.placements, 'doc.placements', placementReg),
  }
}

// Rust strings order by UTF-8 bytes, equivalently Unicode scalar values, not
// JavaScript's UTF-16 units (which sort astral characters before U+E000).
export const compareString = (a: string, b: string): number => {
  if (a === b) return 0
  let i = 0
  let j = 0
  while (i < a.length && j < b.length) {
    const left = a.codePointAt(i)
    const right = b.codePointAt(j)
    // The loop bounds guarantee both code points exist.
    if (left === undefined || right === undefined) throw new RangeError('string comparison index out of bounds')
    if (left !== right) return left < right ? -1 : 1
    i += left > 0xffff ? 2 : 1
    j += right > 0xffff ? 2 : 1
  }
  return i === a.length ? -1 : 1
}
export const compareStamp = (a: Stamp, b: Stamp): number => a[0] - b[0] || a[1] - b[1] || compareString(a[2], b[2])
const compareOptional = (a: string | null, b: string | null): number => a === b ? 0 : a === null ? -1 : b === null ? 1 : compareString(a, b)
const compareName = (a: NameReg, b: NameReg): number => compareStamp(a.at, b.at) || compareString(a.value, b.value)
const comparePosition = (a: PositionReg, b: PositionReg): number => compareStamp(a.at, b.at) || compareOptional(a.parent, b.parent) || compareString(a.key, b.key)
const comparePlacement = (a: PlacementReg, b: PlacementReg): number => compareStamp(a.at, b.at) || compareOptional(a.folder, b.folder) || compareString(a.key, b.key)
const own = <TValue,>(map: Record<string, TValue>, key: string): TValue | undefined => Object.hasOwn(map, key) ? map[key] : undefined
const set = <TValue,>(map: Record<string, TValue>, key: string, value: TValue): Record<string, TValue> => Object.defineProperty(map, key, { value, writable: true, configurable: true, enumerable: true })
const copyReg = <TReg extends NameReg | PositionReg | PlacementReg>(reg: TReg): TReg => ({ ...reg, at: [...reg.at] })
const put = <TField extends string, TValue>(regs: Partial<Record<TField, TValue>>, field: TField, value: TValue, compare: (a: TValue, b: TValue) => number, clone: (value: TValue) => TValue): boolean => {
  const old = regs[field]
  if (old !== undefined && compare(value, old) <= 0) return false
  regs[field] = clone(value)
  return true
}
const joinFolder = (doc: FolderDoc, id: FolderId, other: FolderRegs): boolean => {
  let regs = own(doc.folders, id)
  let changed = regs === undefined
  if (regs === undefined) {
    regs = {}
    set(doc.folders, id, regs)
  }
  if (other.deleted !== undefined) changed = put(regs, 'deleted', other.deleted, compareStamp, (at): Stamp => [...at]) || changed
  if (regs.deleted !== undefined) {
    if (regs.name !== undefined) {
      delete regs.name
      changed = true
    }
  } else if (other.name !== undefined) changed = put(regs, 'name', other.name, compareName, copyReg) || changed
  if (other.position !== undefined) changed = put(regs, 'position', other.position, comparePosition, copyReg) || changed
  return changed
}
const joinPlacement = (doc: FolderDoc, agent: AgentSubject, reg: PlacementReg): boolean => {
  const old = own(doc.placements, agent)
  if (old !== undefined && comparePlacement(reg, old) <= 0) return false
  set(doc.placements, agent, copyReg(reg))
  return true
}
/** Mutates doc; per-register maximum, with permanent tombstones and content tie-breaks. */
export const mergeDoc = (doc: FolderDoc, other: FolderDoc): boolean => {
  let changed = false
  for (const [id, regs] of Object.entries(other.folders)) changed = joinFolder(doc, id, regs) || changed
  for (const [agent, reg] of Object.entries(other.placements)) changed = joinPlacement(doc, agent, reg) || changed
  return changed
}
export const isLive = (doc: FolderDoc, id: FolderId): boolean => {
  const regs = own(doc.folders, id)
  return regs !== undefined && regs.deleted === undefined && regs.position !== undefined
}
export const positionKey = (doc: FolderDoc, id: FolderId): string => own(doc.folders, id)?.position?.key ?? ''
export const liveAncestor = (doc: FolderDoc, start: FolderId | null): FolderId | null => {
  let at = start
  const count = Object.keys(doc.folders).length
  for (let i = 0; at !== null && i <= count; i += 1) {
    const regs = own(doc.folders, at)
    if (regs === undefined) return null
    if (isLive(doc, at)) return at
    at = regs.position?.parent ?? null
  }
  return null
}
const livePosition = (doc: FolderDoc, id: FolderId): PositionReg => {
  const position = own(doc.folders, id)?.position
  if (position === undefined) throw new FolderValidationError(`doc.folders[${JSON.stringify(id)}].position`, 'a live folder position')
  return position
}
export const drawnParents = (doc: FolderDoc): Map<FolderId, FolderId | null> => {
  const parents = new Map<FolderId, FolderId | null>(Object.entries(doc.folders).filter(([id]) => isLive(doc, id)).map(([id]) => [id, liveAncestor(doc, livePosition(doc, id).parent)]))
  const done = new Set<FolderId>()
  for (const start of parents.keys()) {
    const path: FolderId[] = []
    const seen = new Map<FolderId, number>()
    let at: FolderId | null = start
    while (at !== null && !done.has(at)) {
      if (seen.has(at)) {
        const cycle = path.slice(seen.get(at))
        const breaker = cycle.reduce((best, id) => (compareStamp(livePosition(doc, id).at, livePosition(doc, best).at) || compareString(id, best)) > 0 ? id : best)
        parents.set(breaker, null)
        break
      }
      seen.set(at, path.length)
      path.push(at)
      at = parents.get(at) ?? null
    }
    for (const id of path) done.add(id)
  }
  return parents
}
export const isFolderWriter = (actor: unknown): boolean => typeof actor === 'string' && ['person/', 'agent/'].some((prefix) => actor.startsWith(prefix) && actor.length > prefix.length)
/** Ignores malformed, future-format, unrelated, daemon, and anonymous claims. */
export const decodeClaim = (claim: unknown): FolderDoc | undefined => {
  if (!isObject(claim) || claim.kind !== claimKind || !isFolderWriter(claim.actor) || !isObject(claim.body) || !isObject(claim.body.fields) || claim.body.fields.v !== claimFormat) return undefined
  try { return decodeDoc(claim.body.fields) } catch (error) {
    if (error instanceof FolderValidationError) return undefined
    throw error
  }
}
