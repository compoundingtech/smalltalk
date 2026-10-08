/**
 * Finds every schema-declared instant in a wire value: `x-st-codec: timestamp` (via `$ref:
 * Timestamp`) and `x-st-codec: epoch-ms`. Branches of `oneOf`/`anyOf` and `if`/`then` are chosen by
 * a structural match (types, consts, enums, required keys, patterns).
 */
import { readFileSync } from 'node:fs'

import { escapePointerToken, type TimePointer } from '../src/kit/time.ts'

interface SchemaObject {
  readonly $ref?: string
  readonly type?: string | readonly string[]
  readonly const?: unknown
  readonly enum?: readonly unknown[]
  readonly pattern?: string
  readonly required?: readonly string[]
  readonly properties?: Readonly<Record<string, Node>>
  readonly additionalProperties?: Node
  readonly items?: Node
  readonly allOf?: readonly Node[]
  readonly oneOf?: readonly Node[]
  readonly anyOf?: readonly Node[]
  readonly if?: Node
  readonly then?: Node
  readonly 'x-st-codec'?: string
}
type Node = SchemaObject | boolean

export const SCHEMA_PATH = new URL('../../../../docs/st3/client-v0/schemas/client-v0.schema.json', import.meta.url)

export const loadDefinitions = (): Record<string, Node> =>
  (JSON.parse(readFileSync(SCHEMA_PATH, 'utf8')) as { $defs: Record<string, Node> }).$defs

const typeOf = (value: unknown): string =>
  value === null ? 'null' : Array.isArray(value) ? 'array' : Number.isInteger(value) ? 'integer' : typeof value

export const makeInstantFinder = (defs: Record<string, Node>) => {
  const deref = (node: Node): Node => {
    if (typeof node === 'boolean' || node.$ref === undefined) return node
    const name = String(node.$ref).replace('#/$defs/', '')
    const { $ref: _ref, ...rest } = node
    const target = defs[name]
    if (target === undefined) throw new Error(`unknown $ref ${node.$ref}`)
    return Object.keys(rest).length === 0 ? target : { allOf: [target, rest] }
  }

  const matches = (raw: Node, value: unknown): boolean => {
    const node = deref(raw)
    if (typeof node === 'boolean') return node
    const actual = typeOf(value)
    if (node.type !== undefined) {
      const types = typeof node.type === 'string' ? [node.type] : node.type
      const ok = types.some((type) => type === actual || (type === 'number' && actual === 'integer'))
      if (!ok) return false
    }
    if ('const' in node && node.const !== value) return false
    if (Array.isArray(node.enum) && !node.enum.includes(value)) return false
    if (typeof node.pattern === 'string' && typeof value === 'string' && !new RegExp(node.pattern, 'u').test(value)) return false
    if (actual === 'object') {
      const record = value as Record<string, unknown>
      for (const key of node.required ?? []) if (!(key in record)) return false
      for (const [key, sub] of Object.entries(node.properties ?? {})) {
        if (key in record && typeof sub !== 'boolean' && ('const' in sub || sub.enum !== undefined) && !matches(sub, record[key])) return false
      }
    }
    if (node.allOf !== undefined && !node.allOf.every((sub) => matches(sub, value))) return false
    if (node.oneOf !== undefined && !node.oneOf.some((sub) => matches(sub, value))) return false
    if (node.anyOf !== undefined && !node.anyOf.some((sub) => matches(sub, value))) return false
    return true
  }

  const walk = (raw: Node, value: unknown, pointer: string, out: Map<string, TimePointer>): void => {
    if (typeof raw === 'boolean' || value === undefined) return
    if (raw.$ref !== undefined) {
      const { $ref: _ref, ...rest } = raw
      walk(defs[String(raw.$ref).replace('#/$defs/', '')]!, value, pointer, out)
      if (Object.keys(rest).length > 0) walk(rest, value, pointer, out)
      return
    }
    const node = raw
    if (node['x-st-codec'] === 'timestamp' && typeof value === 'string') {
      out.set(pointer, { pointer, codec: 'timestamp' })
      return
    }
    if (node['x-st-codec'] === 'epoch-ms' && Number.isInteger(value)) {
      out.set(pointer, { pointer, codec: 'epoch-ms' })
      return
    }
    if (value === null) return
    for (const sub of node.allOf ?? []) walk(sub, value, pointer, out)
    if (node.if !== undefined && matches(node.if, value) && node.then !== undefined) walk(node.then, value, pointer, out)
    for (const key of ['oneOf', 'anyOf'] as const) {
      const branch = node[key]?.find((sub) => matches(sub, value))
      if (branch !== undefined) walk(branch, value, pointer, out)
    }
    if (Array.isArray(value)) {
      const items = node.items
      if (items !== undefined) value.forEach((item, index) => walk(items, item, `${pointer}/${index}`, out))
      return
    }
    if (typeof value === 'object') {
      const properties = node.properties ?? {}
      for (const [key, item] of Object.entries(value as Record<string, unknown>)) {
        const sub = properties[key] ?? (typeof node.additionalProperties === 'object' ? node.additionalProperties : undefined)
        if (sub !== undefined) walk(sub, item, `${pointer}/${escapePointerToken(key)}`, out)
      }
    }
  }

  /** Instants inside `value`, a value of schema definition `definition`, below `pointer`. */
  return (definition: string, value: unknown, pointer: string): TimePointer[] => {
    const out = new Map<string, TimePointer>()
    walk({ $ref: `#/$defs/${definition}` }, value, pointer, out)
    return [...out.values()]
  }
}
