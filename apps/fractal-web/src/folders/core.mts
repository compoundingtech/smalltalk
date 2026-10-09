// Wire and reducer authority: fractal/src/folders.rs and io/folder_sync/core.rs.
// No Node imports: the server and browser share the same register semantics.
// Milliseconds must be lossless JavaScript integers; counters have Rust's u32 range.
export type Stamp = [milliseconds: number, counter: number, writer: string]
export type FolderId = string
export type AgentSubject = string
export interface NameReg { value: string; at: Stamp }
export interface PositionReg { parent: FolderId | null; key: string; at: Stamp }
export interface PlacementReg { folder: FolderId | null; key: string; at: Stamp }
export interface FolderRegs { name?: NameReg; position?: PositionReg; deleted?: Stamp }
export interface FolderDoc { folders: Record<FolderId, FolderRegs>; placements: Record<AgentSubject, PlacementReg> }
export interface CreateFolderOp { op: 'create_folder'; id: FolderId; name: string; parent: FolderId | null; key: string; at: Stamp }
export interface RenameFolderOp { op: 'rename_folder'; id: FolderId; name: string; at: Stamp }
export interface MoveFolderOp { op: 'move_folder'; id: FolderId; parent: FolderId | null; key: string; at: Stamp }
export interface DeleteFolderOp { op: 'delete_folder'; id: FolderId; at: Stamp }
export interface PlaceOp { op: 'place'; agent: AgentSubject; folder: FolderId | null; key: string; at: Stamp }
export type FolderOp = CreateFolderOp | RenameFolderOp | MoveFolderOp | DeleteFolderOp | PlaceOp
export interface ProjectedFolder { id: FolderId; name: string; key: string; folders: readonly ProjectedFolder[]; members: readonly AgentSubject[] }
export interface Projection { folders: ProjectedFolder[]; unfiled: AgentSubject[] }
export interface Insertion { key: string; rekeyed: [index: number, key: string][] }
export interface ClaimFields extends FolderDoc { v: 1 }

export const claimKind = 'custom.fractal.sidebar'
export const claimFormat = 1
const maxCounter = 0xffffffff
const digits = '0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz'
const encoder = new TextEncoder()

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

export const createFolder = (id: FolderId, name: string, parent: FolderId | null, key: string, at: Stamp): CreateFolderOp => ({ op: 'create_folder', id, name, parent, key, at: [...at] })
export const renameFolder = (id: FolderId, name: string, at: Stamp): RenameFolderOp => ({ op: 'rename_folder', id, name, at: [...at] })
export const moveFolder = (id: FolderId, parent: FolderId | null, key: string, at: Stamp): MoveFolderOp => ({ op: 'move_folder', id, parent, key, at: [...at] })
export const deleteFolder = (id: FolderId, at: Stamp): DeleteFolderOp => ({ op: 'delete_folder', id, at: [...at] })
export const place = (agent: AgentSubject, folder: FolderId | null, key: string, at: Stamp): PlaceOp => ({ op: 'place', agent, folder, key, at: [...at] })

/** Input is the ops array, not the POST envelope; output has canonical Rust field order. */
export const decodeOps = (value: unknown): FolderOp[] => {
  if (!isArray(value)) throw new FolderValidationError('ops', 'an array')
  return value.map((value, index) => {
    const path = `ops[${index}]`
    const op = object(value, path)
    const at = stamp(op.at, `${path}.at`)
    if (op.op === 'place') {
      return place(string(op.agent, `${path}.agent`), optionalId(op.folder, `${path}.folder`), string(op.key, `${path}.key`), at)
    }
    const id = string(op.id, `${path}.id`)
    switch (op.op) {
      case 'create_folder': return createFolder(id, string(op.name, `${path}.name`), optionalId(op.parent, `${path}.parent`), string(op.key, `${path}.key`), at)
      case 'rename_folder': return renameFolder(id, string(op.name, `${path}.name`), at)
      case 'move_folder': return moveFolder(id, optionalId(op.parent, `${path}.parent`), string(op.key, `${path}.key`), at)
      case 'delete_folder': return deleteFolder(id, at)
      default: throw new FolderValidationError(`${path}.op`, 'a folder operation tag')
    }
  })
}

// Rust strings order by UTF-8 bytes, equivalently Unicode scalar values, not
// JavaScript's UTF-16 units (which sort astral characters before U+E000).
const compareString = (a: string, b: string): number => {
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

// These reducers mutate trusted, decoded documents and copy every incoming
// register: neither the op nor the other replica aliases the resulting state.
/** Mutates doc; returns whether a register changed; incoming register storage is copied. */
export const applyOp = (doc: FolderDoc, op: FolderOp): boolean => {
  switch (op.op) {
    case 'create_folder': return joinFolder(doc, op.id, { name: { value: op.name, at: op.at }, position: { parent: op.parent, key: op.key, at: op.at } })
    case 'rename_folder': return joinFolder(doc, op.id, { name: { value: op.name, at: op.at } })
    case 'move_folder': return joinFolder(doc, op.id, { position: { parent: op.parent, key: op.key, at: op.at } })
    case 'delete_folder': return joinFolder(doc, op.id, { deleted: op.at })
    case 'place': return joinPlacement(doc, op.agent, { folder: op.folder, key: op.key, at: op.at })
    default: throw new FolderValidationError('op.op', 'a folder operation tag')
  }
}
/** Mutates doc; per-register maximum, with permanent tombstones and content tie-breaks. */
export const mergeDoc = (doc: FolderDoc, other: FolderDoc): boolean => {
  let changed = false
  for (const [id, regs] of Object.entries(other.folders)) changed = joinFolder(doc, id, regs) || changed
  for (const [agent, reg] of Object.entries(other.placements)) changed = joinPlacement(doc, agent, reg) || changed
  return changed
}
export const maxStamp = (doc: FolderDoc): Stamp | undefined => {
  let max: Stamp | undefined
  const visit = (at: Stamp | undefined): void => { if (at !== undefined && (max === undefined || compareStamp(at, max) > 0)) max = at }
  for (const regs of Object.values(doc.folders)) {
    visit(regs.name?.at)
    visit(regs.position?.at)
    visit(regs.deleted)
  }
  for (const reg of Object.values(doc.placements)) visit(reg.at)
  return max === undefined ? undefined : [...max]
}

export class Hlc {
  #writer: string
  #lastMs = 0
  #counter = 0
  constructor(writer: string) { this.#writer = string(writer, 'writer') }
  observe(value: Stamp): void {
    const at = stamp(value, 'at')
    if (at[0] > this.#lastMs) {
      this.#lastMs = at[0]
      this.#counter = at[1]
    } else if (at[0] === this.#lastMs) this.#counter = Math.max(this.#counter, at[1])
  }
  stamp(wallMs: number): Stamp {
    integer(wallMs, 'wallMs')
    if (wallMs > this.#lastMs) {
      this.#lastMs = wallMs
      this.#counter = 0
    } else if (this.#counter === maxCounter) {
      this.#lastMs = integer(this.#lastMs + 1, 'hlcMs')
      this.#counter = 0
    } else this.#counter += 1
    return [this.#lastMs, this.#counter, this.#writer]
  }
}

/** Rust UUIDv7 construction; caller supplies exactly 10 random bytes and wall clock. */
export const folderId = (unixMs: number, random: Uint8Array): FolderId => {
  integer(unixMs, 'unixMs')
  if (!(random instanceof Uint8Array) || random.length !== 10) throw new FolderValidationError('random', '10 random bytes')
  const [first, second, third] = random
  if (first === undefined || second === undefined || third === undefined) throw new FolderValidationError('random', '10 random bytes')
  const bytes = new Uint8Array(16)
  let ms = BigInt(unixMs)
  for (let i = 5; i >= 0; i -= 1) { bytes[i] = Number(ms & 255n); ms >>= 8n }
  bytes[6] = 0x70 | (first & 0x0f)
  bytes[7] = second
  bytes[8] = 0x80 | (third & 0x3f)
  bytes.set(random.subarray(3), 9)
  return Array.from(bytes, (byte, index) => `${[4, 6, 8, 10].includes(index) ? '-' : ''}${byte.toString(16).padStart(2, '0')}`).join('')
}
const digit = (byte: number): number => byte >= 48 && byte <= 57 ? byte - 48 : byte >= 65 && byte <= 90 ? byte - 65 + 10 : byte >= 97 && byte <= 122 ? byte - 97 + 36 : 0
export const keyBetween = (lower: string | null = null, upper: string | null = null): string => {
  const aText = (lower ?? '').replace(/0+$/, '')
  const bText = upper?.replace(/0+$/, '')
  const a = encoder.encode(aText)
  let b = bText && compareString(aText, bText) < 0 ? encoder.encode(bText) : undefined
  let out = ''
  for (let i = 0; ; i += 1) {
    const aByte = a[i]
    const exhausted = aByte === undefined
    const da = aByte === undefined ? 0 : digit(aByte)
    // Rust indexes b[i] here: retain its failure on an invalid upper key
    // rather than treating an out-of-bounds byte as zero forever.
    const bByte = b?.[i]
    if (b !== undefined && bByte === undefined) throw new RangeError('upper order key exhausted its shared prefix')
    const db = bByte === undefined ? 62 : digit(bByte)
    if (da === db) { out += digits[da]; continue }
    if (db > da + 1) {
      out += digits[exhausted && b !== undefined ? db - 1 : !exhausted && b === undefined ? da + 1 : Math.floor((da + db) / 2)]
      return out
    }
    if (b !== undefined && b.length > i + 1) return out + digits[db]
    out += digits[da]
    b = undefined
  }
}
const spread = (lower: string | null | undefined, upper: string | null, count: number): string[] | undefined => {
  const keys: string[] = []
  for (let i = 0; i < count; i += 1) {
    const after = keys.at(-1) ?? lower
    let key = keyBetween(after, upper)
    if (after != null && compareString(key, after) <= 0) key = `${after}V`
    if ((after != null && compareString(after, key) >= 0) || (upper != null && compareString(key, upper) >= 0)) return undefined
    keys.push(key)
  }
  return keys
}
/** Siblings are in projected order, with the moved item removed. */
export const insertAt = (siblings: readonly string[], index: number): Insertion => {
  integer(index, 'index', siblings.length)
  const lower = index > 0 ? siblings[index - 1] : null
  let best: Insertion | undefined
  for (let end = index; end <= siblings.length; end += 1) {
    const keys = spread(lower, siblings[end] ?? null, end - index + 1)
    const key = keys?.[0]
    if (keys !== undefined && key !== undefined) { best = { key, rekeyed: keys.slice(1).map((key, offset) => [index + offset, key]) }; break }
  }
  const forward = best?.rekeyed.length ?? Infinity
  for (let start = index - 1; start >= 0 && index - start < forward; start -= 1) {
    const keys = spread(start > 0 ? siblings[start - 1] : null, siblings[index] ?? null, index - start + 1)
    const key = keys?.at(-1)
    if (keys !== undefined && key !== undefined) { best = { key, rekeyed: keys.slice(0, -1).map((key, offset) => [start + offset, key]) }; break }
  }
  // The forward search always reaches an unbounded upper key and succeeds.
  if (best === undefined) throw new RangeError('insertion could not find an unbounded order key')
  return best
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
export const wouldCycle = (doc: FolderDoc, id: FolderId, parent: FolderId | null): boolean => {
  const parents = drawnParents(doc)
  let at = parent
  for (let i = 0; at !== null && i <= parents.size; i += 1) {
    if (at === id) return true
    at = parents.get(at) ?? null
  }
  return false
}
/** Legacy roots retain caller order; native arrangements opt into key/subject ordering. */
export const project = (doc: FolderDoc, live: Iterable<AgentSubject>, { rootOrder }: {
  readonly rootOrder?: 'placement'
} = {}): Projection => {
  const children = new Map<FolderId | null, FolderId[]>()
  for (const [id, parent] of drawnParents(doc)) {
    let siblings = children.get(parent)
    if (siblings === undefined) { siblings = []; children.set(parent, siblings) }
    siblings.push(id)
  }
  const members = new Map<FolderId, [key: string, agent: AgentSubject][]>()
  const unfiled: AgentSubject[] = []
  for (const agent of live) {
    const placement = own(doc.placements, agent)
    const folder = placement === undefined ? null : liveAncestor(doc, placement.folder)
    if (folder === null || placement === undefined) unfiled.push(agent)
    else {
      let placements = members.get(folder)
      if (placements === undefined) { placements = []; members.set(folder, placements) }
      placements.push([placement.key, agent])
    }
  }
  if (rootOrder === 'placement') unfiled.sort((left, right) => {
    const a = own(doc.placements, left)
    const b = own(doc.placements, right)
    if (a === undefined) return b === undefined ? compareString(left, right) : 1
    if (b === undefined) return -1
    return compareString(a.key, b.key) || compareString(left, right)
  })
  const subtree = (parent: FolderId | null): ProjectedFolder[] => (children.get(parent) ?? [])
    .sort((a, b) => compareString(positionKey(doc, a), positionKey(doc, b)) || compareString(a, b))
    .map((id) => ({
      id,
      name: own(doc.folders, id)?.name?.value ?? '',
      key: positionKey(doc, id),
      folders: subtree(id),
      members: (members.get(id) ?? []).sort((a, b) => compareString(a[0], b[0]) || compareString(a[1], b[1])).map(([, agent]) => agent),
    }))
  return { folders: subtree(null), unfiled }
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
export const claimFields = (doc: FolderDoc): ClaimFields => ({ ...decodeDoc(doc), v: claimFormat })
// Op serialization follows serde's enum/struct declaration order; object
// insertion order on an incoming HTTP request must not change its batch key.
export const serializeOps = (ops: readonly FolderOp[]): string => JSON.stringify(decodeOps(ops))
// serde_json::Map sorts *all* object keys by Rust string order. A custom
// serializer also keeps numeric-looking map keys out of JS's integer ordering.
const sortedObjectJson = (value: Record<string, unknown>): string => `{${Object.keys(value).sort(compareString).map((key) => `${JSON.stringify(key)}:${sortedJson(value[key])}`).join(',')}}`
const sortedJson = (value: unknown): string | undefined => {
  if (isArray(value)) return `[${value.map(sortedJson).join(',')}]`
  if (isObject(value)) return sortedObjectJson(value)
  return JSON.stringify(value)
}
/** Canonical serde_json::Map serialization, used for republish idempotency. */
export const serializeFields = (doc: FolderDoc): string => sortedObjectJson(object(claimFields(doc), 'fields'))
const digest = async (text: string): Promise<string> => {
  const hash = new Uint8Array(await globalThis.crypto.subtle.digest('SHA-256', encoder.encode(text)))
  return Array.from(hash.subarray(0, 16), (byte) => byte.toString(16).padStart(2, '0')).join('')
}
/** SHA-256 truncated to 16 bytes; uses standard Web Crypto, never a Node import. */
export const opsKey = async (subject: string, ops: readonly FolderOp[]): Promise<string> => `fractal.sidebar:${subject}:ops:${await digest(serializeOps(ops))}`
export const docKey = async (subject: string, doc: FolderDoc): Promise<string> => `fractal.sidebar:${subject}:doc:${await digest(serializeFields(doc))}`
