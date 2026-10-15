import { Agent, CollectionName, Resource, UsageSummary, decodeUnknownSync } from '@smalltalk/st3-client/schema'
import { Schema, SchemaAST, SchemaTransformation } from 'effect'
import type { Brand, DateTime, Duration, Option } from 'effect'
import type { Issue } from 'effect/SchemaIssue'

/** Rejected rows have no usable value; degraded rows retain an open, unrecognized leaf. */
export interface ResourceDecodeDiagnostic {
  readonly _tag: 'Rejected' | 'Degraded'
  readonly id?: string
  readonly path: string
  readonly reason: string
}
/** Compatibility name for the single tagged diagnostic stream. */
export type Undecodable = ResourceDecodeDiagnostic
export interface DecodeDiagnosticCounts {
  readonly rejectedCount: number
  readonly degradedCount: number
}
export interface Unrecognized {
  readonly _tag: 'Unrecognized'
  readonly raw: unknown
}
export interface UnrecognizedLeaf {
  readonly path: string
  readonly value: Unrecognized
}

/** Open literal leaves, preserving branded identities, rich values and union discriminators. */
export type Open<T> = [T] extends [boolean] ? boolean extends T ? T : T | Unrecognized
  : T extends Brand.Brand<string> | DateTime.Utc | Duration.Duration ? T
  : T extends { readonly _tag: 'Unknown'; readonly raw: string } ? Unrecognized
  : T extends string ? string extends T ? T : T extends `${string}/${string}` ? T : T | Unrecognized
  : T extends number ? number extends T ? T : T | Unrecognized
  : T extends boolean ? boolean extends T ? T : T | Unrecognized
  : T extends Option.Option<infer TValue> ? Option.Option<Open<TValue>>
  : T extends readonly (infer TValue)[] ? readonly Open<TValue>[]
  : T extends object ? { readonly [TKey in keyof T]: TKey extends 'kind' | '_tag' | 'type' | 'op' ? T[TKey] : Open<T[TKey]> }
  : T

type DecodeMetadata = { readonly decode?: { readonly unrecognized: readonly UnrecognizedLeaf[] } }
export type DecodedAgent = Open<Agent> & DecodeMetadata
export type DecodedUsageSummary = Open<UsageSummary>
/** Only roster resources opt into open leaves; other resources remain generated-codec checked. */
export type DecodedResource = (Exclude<Resource, Agent> | Open<Agent>) & DecodeMetadata
export interface ResourceDecodeResult {
  readonly value?: DecodedResource
  readonly undecodable: readonly Undecodable[]
}

const object = (value: unknown): value is Record<string, unknown> =>
  typeof value === 'object' && value !== null && !Array.isArray(value)
const validators = new WeakMap<SchemaAST.AST, (value: unknown) => boolean>()
const accepts = (ast: SchemaAST.AST, value: unknown): boolean => {
  let validate = validators.get(ast)
  if (validate === undefined) {
    validate = Schema.is(Schema.make<Schema.Codec<unknown, unknown>>(ast))
    validators.set(ast, validate)
  }
  return validate(value)
}

const UnrecognizedSchema = Schema.Struct({ _tag: Schema.Literal('Unrecognized'), raw: Schema.Unknown })
const widened = new WeakMap<SchemaAST.AST, SchemaAST.AST>()

/** AST nodes carry non-public parser methods on their prototype. Preserve those methods
 * when replacing link/context fields not exposed by this Effect version's public API. */
const rebuildNode = (ast: SchemaAST.AST, fields: Partial<Pick<SchemaAST.AST, 'encoding' | 'context'>>): SchemaAST.AST =>
  Object.create(Object.getPrototypeOf(ast), Object.getOwnPropertyDescriptors({ ...ast, ...fields }))
/** Rebuild both sides of codec links, retaining generated rich transformations and checks.
 * Unknown primitive cases are real tagged values, never fabricated known literal values.
 */
const openLeaves = (ast: SchemaAST.AST): SchemaAST.AST => {
  const cached = widened.get(ast)
  if (cached !== undefined) return cached
  let result = ast
  if (ast._tag === 'Literal' || ast._tag === 'Enum'
    || ast._tag === 'Union' && ast.types.every((type) => type._tag === 'Literal')) {
    const unknown = Schema.Unknown.check(Schema.makeFilter((raw: unknown) =>
      (typeof raw === 'string' || typeof raw === 'number' || typeof raw === 'boolean')
        && (ast._tag === 'Literal' ? typeof raw === typeof ast.literal
          : ast._tag === 'Union' ? ast.types.some((type) => type._tag === 'Literal' && typeof raw === typeof type.literal)
          : typeof raw === 'string' || typeof raw === 'number')
        && !accepts(ast, raw),
    { expected: 'an unrecognized primitive literal' })).pipe(Schema.decodeTo(
      UnrecognizedSchema,
      SchemaTransformation.transform({
        decode: (raw: unknown): Unrecognized => ({ _tag: 'Unrecognized', raw }),
        encode: (value: Unrecognized) => value.raw,
      }),
    ))
    const known = ast.encoding === undefined ? ast : rebuildNode(ast, { encoding: undefined })
    const union = Schema.Union([Schema.make<Schema.Codec<unknown, unknown>>(known), unknown]).ast
    result = rebuildNode(ast.encoding === undefined ? union : SchemaAST.toType(union), { context: ast.context })
  } else if (ast._tag === 'Objects') {
    result = new SchemaAST.Objects(
      ast.propertySignatures.map((property) => new SchemaAST.PropertySignature(property.name,
        property.name === 'kind' || property.name === '_tag' || property.name === 'type' || property.name === 'op'
          ? property.type : ast.encoding === undefined ? openLeaves(property.type) : SchemaAST.toType(openLeaves(property.type)))),
      ast.indexSignatures.map((index) => new SchemaAST.IndexSignature(index.parameter, openLeaves(index.type))),
      ast.annotations, ast.checks, ast.encoding, ast.context, ast.encodingChecks,
    )
  } else if (ast._tag === 'Arrays') {
    result = new SchemaAST.Arrays(ast.isMutable,
      ast.elements.map((type) => ast.encoding === undefined ? openLeaves(type) : SchemaAST.toType(openLeaves(type))),
      ast.rest.map((type) => ast.encoding === undefined ? openLeaves(type) : SchemaAST.toType(openLeaves(type))),
      ast.annotations, ast.checks, ast.encoding, ast.context, ast.encodingChecks)
  } else if (ast._tag === 'Union') {
    result = new SchemaAST.Union(ast.types.map((type) => ast.encoding === undefined ? openLeaves(type) : SchemaAST.toType(openLeaves(type))), ast.options,
      ast.annotations, ast.checks, ast.encoding, ast.context, ast.encodingChecks)
  } else if (ast._tag === 'Declaration') {
    result = new SchemaAST.Declaration(ast.typeParameters.map((type) => SchemaAST.toType(openLeaves(type))), ast.run,
      ast.annotations, ast.checks, ast.encoding, ast.context, ast.encodingChecks, ast.encodingRun)
  }
  if (ast.encoding !== undefined) {
    const link = (value: SchemaAST.Link) => new SchemaAST.Link(openLeaves(value.to), value.transformation)
    result = rebuildNode(result, { encoding: [link(ast.encoding[0]), ...ast.encoding.slice(1).map(link)] })
  }
  widened.set(ast, result)
  return result
}

// The type is the recursive counterpart of the AST transformation above. No wire schema changes.
const AgentSchema = Schema.make<Schema.Codec<DecodedAgent, unknown>>(openLeaves(Agent.ast))
const UsageSchema = Schema.make<Schema.Codec<DecodedUsageSummary, unknown>>(openLeaves(UsageSummary.ast))
const decodeAgent = Schema.decodeUnknownSync(AgentSchema)
const decodeResource = decodeUnknownSync(Resource)
export const decodedUsageEquivalence: (left: DecodedUsageSummary, right: DecodedUsageSummary) => boolean =
  Schema.toEquivalence(UsageSchema)

type PathNode = Map<string, PathNode>
const schemaPaths: PathNode = new Map()
const collectPaths = (ast: SchemaAST.AST, node: PathNode, ancestors: ReadonlySet<SchemaAST.AST>): void => {
  if (ancestors.has(ast)) return
  const next = new Set(ancestors).add(ast)
  const child = (key: string): PathNode => {
    const existing = node.get(key)
    if (existing !== undefined) return existing
    const created: PathNode = new Map()
    node.set(key, created)
    return created
  }
  if (ast._tag === 'Objects') {
    for (const property of ast.propertySignatures)
      collectPaths(property.type, child(String(property.name)), next)
    for (const index of ast.indexSignatures) collectPaths(index.type, child('*'), next)
  } else if (ast._tag === 'Arrays') {
    for (const type of [...ast.elements, ...ast.rest]) collectPaths(type, child('*'), next)
  } else if (ast._tag === 'Union') {
    for (const type of ast.types) collectPaths(type, node, next)
  } else if (ast._tag === 'Declaration') {
    for (const type of ast.typeParameters) collectPaths(type, node, next)
  }
}
collectPaths(SchemaAST.toEncoded(Resource.ast), schemaPaths, new Set())

/** Only schema-owned names leave the SDK. Array indices and record keys become `*`. */
export const normalizeDecodePath = (path: string): string => {
  if (path === '$') return path
  let node = schemaPaths
  const normalized: string[] = []
  for (const segment of path.split('.')) {
    const key = node.has(segment) ? segment : '*'
    const child = node.get(key)
    if (child === undefined) return [...normalized, '*'].join('.')
    normalized.push(key)
    node = child
  }
  return normalized.length === 0 ? '$' : normalized.join('.')
}

const unknownLeaves = (value: unknown, path: string, found: UnrecognizedLeaf[]): void => {
  if (object(value)) {
    if (value._tag === 'Unrecognized') {
      found.push({ path, value: { _tag: 'Unrecognized', raw: value.raw } })
      return
    }
    if (value._tag === 'Some' && Object.hasOwn(value, 'value')) {
      unknownLeaves(value.value, path, found)
      return
    }
    for (const [key, child] of Object.entries(value)) unknownLeaves(child, path ? `${path}.${key}` : key, found)
  } else if (Array.isArray(value)) value.forEach((child, index) => unknownLeaves(child, `${path}.${index}`, found))
}

/** Convert an already decoded generated agent at fixture/SDK integration boundaries. */
export const openAgent = (agent: Agent): DecodedAgent => {
  const value = decodeAgent(Schema.encodeSync(Agent)(agent))
  const unrecognized: UnrecognizedLeaf[] = []
  unknownLeaves(value, '', unrecognized)
  return unrecognized.length === 0 ? value : { ...value, decode: { unrecognized } }
}

/** Identity and structure are closed; future agent leaf cases remain in their own row. */
export const decodeResourceRow = (raw: unknown): ResourceDecodeResult => {
  const id = object(raw) && typeof raw.id === 'string' ? raw.id : undefined
  const diagnostic = (path: string, reason: string, tag: ResourceDecodeDiagnostic['_tag'] = 'Rejected'): Undecodable => ({
    _tag: tag, ...(id === undefined ? {} : { id }), path: normalizeDecodePath(path), reason,
  })
  try {
    if (!object(raw) || raw.kind !== 'agent') {
      const value = decodeResource(raw)
      if (value.kind === 'agent')
        return { undecodable: [diagnostic('kind', 'Resource identity did not select the agent codec')] }
      if (typeof value.kind !== 'string')
        return { undecodable: [diagnostic('kind', 'Unrecognized resource identity discriminator')] }
      return { value, undecodable: [] }
    }
    const value = decodeAgent(raw)
    if (typeof value.kind !== 'string')
      return { undecodable: [diagnostic('kind', 'Unrecognized resource identity discriminator')] }
    const unrecognized: UnrecognizedLeaf[] = []
    unknownLeaves(value, '', unrecognized)
    return {
      value: unrecognized.length > 0 && value.kind === 'agent' ? { ...value, decode: { unrecognized } } : value,
      undecodable: unrecognized.map((leaf) => diagnostic(leaf.path, 'Unrecognized leaf value', 'Degraded')),
    }
  } catch (error) {
    const failures: Undecodable[] = []
    if (object(raw) && raw.kind === 'agent' && Schema.isSchemaError(error)) {
      const visit = (issue: Issue, path: readonly PropertyKey[]): void => {
        if (issue._tag === 'Pointer') visit(issue.issue, [...path, ...issue.path])
        else if ('issues' in issue && issue.issues.length > 0)
          issue.issues.forEach((child) => visit(child, path))
        else if ('issue' in issue) visit(issue.issue, path)
        else failures.push(diagnostic(path.length === 0 ? '$' : path.map(String).join('.'),
          'Value does not satisfy the resource field schema'))
      }
      visit(error.issue, [])
      if (failures.length > 0) return { undecodable: failures }
    }
    const encoded = SchemaAST.toEncoded(Resource.ast)
    const selected = encoded._tag === 'Union' && object(raw) ? encoded.types.find((type) =>
      type._tag === 'Objects' && type.propertySignatures.some((property) => property.name === 'kind'
        && property.type._tag === 'Literal' && property.type.literal === raw.kind)) : undefined
    if (selected?._tag === 'Objects' && object(raw)) {
      for (const property of selected.propertySignatures) {
        const key = String(property.name)
        if (!accepts(property.type, raw[key])) failures.push(diagnostic(key, 'Value does not satisfy the resource field schema'))
      }
    }
    return { undecodable: failures.length > 0 ? failures : [diagnostic('$', 'Value does not satisfy the resource schema')] }
  }
}

export const decodeResourceRows = (rows: readonly unknown[]): {
  readonly items: readonly DecodedResource[]
  readonly undecodable: readonly Undecodable[]
} => {
  const items: DecodedResource[] = []
  const undecodable: Undecodable[] = []
  for (const raw of rows) {
    const decoded = decodeResourceRow(raw)
    if (decoded.value !== undefined) items.push(decoded.value)
    undecodable.push(...decoded.undecodable)
  }
  return { items, undecodable }
}

/** One SDK session owns counters over the finite schema-trie path domain and collection enum.
 * Unknown segments collapse to one wildcard at their nearest schema parent.
 */
export const makeDecodeDiagnostics = (
  event: (resource: string, path: string, count: number, first: boolean,
    counters: DecodeDiagnosticCounts, tag: ResourceDecodeDiagnostic['_tag']) => void,
) => {
  const isCollection = Schema.is(CollectionName)
  const counts = new Map<string, DecodeDiagnosticCounts>()
  return (resource: string, diagnostics: readonly Undecodable[]): void => {
    resource = isCollection(resource) ? resource : '$'
    for (const diagnostic of diagnostics) {
      const path = normalizeDecodePath(diagnostic.path)
      const key = `${resource}:${path}`
      const previous = counts.get(key)
      const counters = {
        rejectedCount: (previous?.rejectedCount ?? 0) + (diagnostic._tag === 'Rejected' ? 1 : 0),
        degradedCount: (previous?.degradedCount ?? 0) + (diagnostic._tag === 'Degraded' ? 1 : 0),
      }
      counts.set(key, counters)
      event(resource, path, counters.rejectedCount + counters.degradedCount, previous === undefined, counters, diagnostic._tag)
    }
  }
}
