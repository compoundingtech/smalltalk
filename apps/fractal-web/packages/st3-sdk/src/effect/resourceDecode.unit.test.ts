import type { Resource as ResourceWire } from '@smalltalk/st3-client'
import { Agent, Resource, decodeUnknownSync } from '@smalltalk/st3-client/schema'
import { Option, SchemaAST } from 'effect'
import { describe, expect, it } from 'vitest'

import oldLiteral from './agentSnapshot.oldLiteral.json'
import { decodeResourceRow, decodeResourceRows, makeDecodeDiagnostics, normalizeDecodePath, decodedUsageEquivalence, openAgent } from './resourceDecode.ts'
import { makeWindowIngest, type ProcessedWindow } from './windowIngest.ts'
import { frame } from './windowIngest.fixture.ts'

const agent = oldLiteral.items[0]!

describe('row-wise resource decoding', () => {
  it('decodes the synthetic old-literal agent snapshot', () => {
    const result = decodeResourceRows(oldLiteral.items)
    expect(result.items).toHaveLength(oldLiteral.items.length)
    expect(result.undecodable).toEqual([])
    expect(result.items[0]).toMatchObject({ id: agent.id, kind: 'agent', name: agent.name })
    const decoded = result.items[0]!
    if (decoded.kind !== 'agent') throw new Error('Expected example agent')
    expect(Option.isSome(decoded.usage)).toBe(true)
    expect(decoded).toEqual(decodeUnknownSync(Agent)(agent))
    expect(openAgent(decodeUnknownSync(Agent)(agent))).toEqual(decoded)
  })

  it('preserves a future aggregation as a real open leaf without fabricating known usage semantics', () => {
    const result = decodeResourceRow({ ...agent, usage: { ...agent.usage, aggregation: 'future-aggregation-v2' } })
    expect(result.value).toMatchObject({
      id: agent.id,
      decode: { unrecognized: [{ path: 'usage.aggregation', value: { _tag: 'Unrecognized', raw: 'future-aggregation-v2' } }] },
    })
    expect(result.undecodable).toEqual([{
      _tag: 'Degraded', id: agent.id, path: 'usage.aggregation',
      reason: 'Unrecognized leaf value',
    }])
    if (result.value?.kind !== 'agent') throw new Error('Expected retained agent')
    expect(Option.isSome(result.value.usage)).toBe(true)
    expect(Option.getOrThrow(result.value.usage).aggregation).toEqual({ _tag: 'Unrecognized', raw: 'future-aggregation-v2' })
  })

  it('keeps broken identity typed and isolated from healthy sibling rows', () => {
    const result = decodeResourceRows([{ ...agent, id: 'not-a-resource-id' }, agent])
    expect(result.items).toHaveLength(1)
    expect(result.items[0]?.id).toBe(agent.id)
    expect(result.undecodable).toContainEqual({
      _tag: 'Rejected', id: 'not-a-resource-id', path: 'id',
      reason: 'Value does not satisfy the resource field schema',
    })
    const missing = decodeResourceRow({ ...agent, id: 42 })
    expect(missing.value).toBeUndefined()
    expect(missing.undecodable).toContainEqual({
      _tag: 'Rejected', path: 'id', reason: 'Value does not satisfy the resource field schema',
    })
  })

  it('opens generated enums without manufacturing a schema-known case', () => {
    const result = decodeResourceRow({ ...agent, state: 'future-state' })
    expect(result.value?.decode?.unrecognized).toContainEqual({
      path: 'state', value: { _tag: 'Unrecognized', raw: 'future-state' },
    })
    expect(result.value).toMatchObject({ state: { _tag: 'Unrecognized', raw: 'future-state' } })
  })

  it('opens other closed literal sets using generated schema metadata', () => {
    const result = decodeResourceRow({ ...agent, observation: 'future-observation' })
    expect(result.value?.decode?.unrecognized).toContainEqual({
      path: 'observation', value: { _tag: 'Unrecognized', raw: 'future-observation' },
    })
    expect(result.value).toMatchObject({ observation: { _tag: 'Unrecognized', raw: 'future-observation' } })
    expect(result.undecodable).toContainEqual({
      _tag: 'Degraded', id: agent.id, path: 'observation',
      reason: 'Unrecognized leaf value',
    })
  })

  it('preserves nested structural failure paths without including row contents', () => {
    const result = decodeResourceRow({ ...agent, runtime_ids: ['broken-runtime-id'] })
    expect(result.value).toBeUndefined()
    expect(result.undecodable).toContainEqual({
      _tag: 'Rejected', id: agent.id, path: 'runtime_ids.*',
      reason: 'Value does not satisfy the resource field schema',
    })
  })

  it('keeps identity discriminators and wrong primitive leaf types closed', () => {
    expect(decodeResourceRow({ ...agent, kind: 'future-agent' })).toMatchObject({
      undecodable: [{ _tag: 'Rejected', path: 'kind' }],
    })
    const malformed = decodeResourceRow({ ...agent, usage: { ...agent.usage, aggregation: 42 } })
    expect(malformed.value).toBeUndefined()
  })

  it('compares unknown usage literals by their real raw value', () => {
    const first = decodeResourceRow({ ...agent, usage: { ...agent.usage, aggregation: 'future-a' } }).value
    const same = decodeResourceRow({ ...agent, usage: { ...agent.usage, aggregation: 'future-a' } }).value
    const changed = decodeResourceRow({ ...agent, usage: { ...agent.usage, aggregation: 'future-b' } }).value
    if (first?.kind !== 'agent' || same?.kind !== 'agent' || changed?.kind !== 'agent')
      throw new Error('Expected retained agents')
    expect(decodedUsageEquivalence(Option.getOrThrow(first.usage), Option.getOrThrow(same.usage))).toBe(true)
    expect(decodedUsageEquivalence(Option.getOrThrow(first.usage), Option.getOrThrow(changed.usage))).toBe(false)
  })

  it('retains the last good items for an all-bad window and recovers on good or empty snapshots', () => {
    const tasks: (() => void)[] = []
    const windows: ProcessedWindow<unknown>[] = []
    const ingest = makeWindowIngest({
      decode: (raw) => decodeResourceRow(raw).value,
      decodeDetailed: decodeResourceRow,
      publish: (window) => windows.push(window),
      now: () => 0,
      schedule: (work) => { tasks.push(work); return () => {} },
    })
    const send = (rows: readonly unknown[]) => {
      // Intentionally inject malformed JSON at the same untrusted wire boundary as the socket.
      ingest.accept(frame(rows as ResourceWire[]))
      while (tasks.length > 0) tasks.shift()!()
    }
    send([agent])
    send([{ ...agent, id: 'broken-id' }])
    expect(windows[1]?.items).toBe(windows[0]?.items)
    expect(windows[1]?.decodeStale).toBe(true)
    expect(windows[1]?.undecodable[0]).toMatchObject({ _tag: 'Rejected', path: 'id' })
    send([agent])
    expect(windows[2]?.decodeStale).toBe(false)
    expect(windows[2]?.undecodable).toEqual([])
    send([])
    expect(windows[3]?.items).toEqual([])
    expect(windows[3]?.decodeStale).toBe(false)
  })

  it.each([42, undefined, null])('retains diagnostics for an identity %s that cannot join server order', (id) => {
    const tasks: (() => void)[] = []
    const windows: ProcessedWindow<unknown>[] = []
    const ingest = makeWindowIngest({
      decode: (raw) => decodeResourceRow(raw).value,
      decodeDetailed: decodeResourceRow,
      publish: (window) => windows.push(window),
      now: () => 0,
      schedule: (work) => { tasks.push(work); return () => {} },
    })
    const good = frame([agent] as ResourceWire[])
    if (good.kind !== 'snapshot') throw new TypeError('Expected a window snapshot')
    const send = (items: readonly unknown[]) => {
      ingest.accept({ ...good, items: items as ResourceWire[] })
      while (tasks.length > 0) tasks.shift()!()
    }
    send([agent])
    const broken = id === null ? null : { ...agent, id }
    send([broken])
    expect(windows[1]?.items).toBe(windows[0]?.items)
    expect(windows[1]?.decodeStale).toBe(true)
    expect(windows[1]?.undecodable).not.toHaveLength(0)
    expect(windows[1]?.rawItems).toEqual([broken])
    ingest.accept({
      kind: 'changes', id: good.id, collection: good.collection,
      upserts: [], removes: [], order: [], has_more: false, snapshot: good.snapshot,
    })
    while (tasks.length > 0) tasks.shift()!()
    expect(windows.at(-1)?.decodeStale).toBe(true)
    expect(windows.at(-1)?.rawItems).toEqual([broken])
    expect(windows.at(-1)?.undecodable).toEqual(windows[1]?.undecodable)
    ingest.accept({
      kind: 'changes', id: good.id, collection: good.collection,
      upserts: [agent] as ResourceWire[], removes: [], order: [agent.id],
      has_more: false, snapshot: good.snapshot,
    })
    while (tasks.length > 0) tasks.shift()!()
    expect(windows.at(-1)?.decodeStale).toBe(true)
    expect(windows.at(-1)?.rawItems).toEqual([agent, broken])
    expect(windows.at(-1)?.undecodable).toEqual(windows[1]?.undecodable)
    send([agent, broken])
    expect(windows.at(-1)?.items).toHaveLength(1)
    expect(windows.at(-1)?.decodeStale).toBe(false)
    expect(windows.at(-1)?.undecodable).not.toHaveLength(0)
    send([agent])
    expect(windows.at(-1)?.decodeStale).toBe(false)
    expect(windows.at(-1)?.undecodable).toEqual([])
  })

  it('clears an unjoinable but identifiable failure only when that row is repaired', () => {
    const tasks: (() => void)[] = []
    const windows: ProcessedWindow<unknown>[] = []
    const ingest = makeWindowIngest({
      decode: (raw) => decodeResourceRow(raw).value,
      decodeDetailed: decodeResourceRow,
      publish: (window) => windows.push(window),
      now: () => 0,
      schedule: (work) => { tasks.push(work); return () => {} },
    })
    const good = frame([agent] as ResourceWire[])
    if (good.kind !== 'snapshot') throw new TypeError('Expected snapshot')
    ingest.accept({ ...good, items: [{ ...agent, runtime_ids: ['broken-id'] }] as ResourceWire[], order: [] })
    while (tasks.length > 0) tasks.shift()!()
    expect(windows.at(-1)?.decodeStale).toBe(true)
    const delta = {
      kind: 'changes' as const, id: good.id, collection: good.collection,
      upserts: [], removes: [], order: [], has_more: false, snapshot: good.snapshot,
    }
    ingest.accept(delta)
    while (tasks.length > 0) tasks.shift()!()
    expect(windows.at(-1)?.decodeStale).toBe(true)
    expect(windows.at(-1)?.undecodable).toEqual(windows[0]?.undecodable)
    ingest.accept({ ...delta, upserts: [agent] as ResourceWire[], order: [agent.id] })
    while (tasks.length > 0) tasks.shift()!()
    expect(windows.at(-1)?.decodeStale).toBe(false)
    expect(windows.at(-1)?.undecodable).toEqual([])
    expect(windows.at(-1)?.items).toHaveLength(1)
  })

  it('counts rejected and degraded diagnostics in one stream per normalized resource/path', () => {
    const observed: unknown[] = []
    const report = makeDecodeDiagnostics((resource, path, count, first, counters, tag) =>
      observed.push({ resource, path, count, first, ...counters, tag }))
    const diagnostic = { _tag: 'Degraded', path: 'usage.aggregation', reason: 'bad leaf' } as const
    report('agents', [diagnostic, { ...diagnostic, _tag: 'Rejected' }])
    report('sessions', [diagnostic])
    report('agents', [{ ...diagnostic, path: 'id' }])
    expect(observed).toEqual([
      { resource: 'agents', path: 'usage.aggregation', count: 1, first: true, rejectedCount: 0, degradedCount: 1, tag: 'Degraded' },
      { resource: 'agents', path: 'usage.aggregation', count: 2, first: false, rejectedCount: 1, degradedCount: 1, tag: 'Rejected' },
      { resource: 'sessions', path: 'usage.aggregation', count: 1, first: true, rejectedCount: 0, degradedCount: 1, tag: 'Degraded' },
      { resource: 'agents', path: 'id', count: 1, first: true, rejectedCount: 0, degradedCount: 1, tag: 'Degraded' },
    ])
  })

  it('normalizes 1000 array indices to one schema path and first-path signal', () => {
    const paths = new Set<string>()
    const firstPaths: string[] = []
    let lastCount = 0
    const report = makeDecodeDiagnostics((_resource, path, count, first) => {
      paths.add(path)
      if (first) firstPaths.push(path)
      lastCount = count
    })
    for (let index = 0; index < 1000; index++)
      report('agents', [{ _tag: 'Rejected', path: `runtime_ids.${index}`, reason: 'bad field' }])
    expect([...paths]).toEqual(['runtime_ids.*'])
    expect(firstPaths).toEqual(['runtime_ids.*'])
    expect(lastCount).toBe(1000)
    expect(normalizeDecodePath('usage.secret-row-id')).toBe('usage.*')
    expect(normalizeDecodePath('receipt.private-row-id')).toBe('receipt.*')
    expect(normalizeDecodePath('receipt.another-row-id')).toBe('receipt.*')
    expect(normalizeDecodePath('runtime_ids.*')).toBe('runtime_ids.*')
  })

  it('keeps separate counters and first-path signals for more than 128 valid schema paths', () => {
    const paths = new Set<string>()
    const visit = (ast: SchemaAST.AST, path: string, ancestors: ReadonlySet<SchemaAST.AST>): void => {
      if (ancestors.has(ast)) return
      const next = new Set(ancestors).add(ast)
      const child = (type: SchemaAST.AST, name: string) => {
        const nested = path ? `${path}.${name}` : name
        paths.add(nested)
        visit(type, nested, next)
      }
      if (ast._tag === 'Objects') {
        for (const property of ast.propertySignatures) child(property.type, String(property.name))
        for (const index of ast.indexSignatures) child(index.type, '*')
      } else if (ast._tag === 'Arrays') {
        for (const type of [...ast.elements, ...ast.rest]) child(type, '*')
      } else if (ast._tag === 'Union') {
        for (const type of ast.types) visit(type, path, next)
      } else if (ast._tag === 'Declaration') {
        for (const type of ast.typeParameters) visit(type, path, next)
      }
    }
    visit(SchemaAST.toEncoded(Resource.ast), '', new Set())
    expect(paths.size).toBeGreaterThan(128)
    const counters = new Map<string, number>()
    const firstPaths: string[] = []
    const report = makeDecodeDiagnostics((resource, path, count, first) => {
      counters.set(`${resource}:${path}`, count)
      if (first) firstPaths.push(`${resource}:${path}`)
    })
    for (const resource of ['agents', 'sessions']) {
      for (const path of paths) {
        expect(normalizeDecodePath(path)).toBe(path)
        const diagnostic = { _tag: 'Rejected', path, reason: 'synthetic failure' } as const
        report(resource, [diagnostic, diagnostic])
      }
    }
    expect(counters.size).toBe(paths.size * 2)
    expect(firstPaths).toHaveLength(paths.size * 2)
    expect(new Set(firstPaths).size).toBe(paths.size * 2)
    expect([...counters.values()].every(count => count === 2)).toBe(true)
  })

  it('bounds unknown paths per schema parent and unknown resources to one bucket', () => {
    const keys = new Set<string>()
    let lastCount = 0
    const report = makeDecodeDiagnostics((resource, path, count) => {
      keys.add(`${resource}:${path}`)
      lastCount = count
    })
    for (let index = 0; index < 1000; index++)
      report(`future-${index}`, [{ _tag: 'Rejected', path: `usage.future-${index}.nested`, reason: 'synthetic failure' }])
    expect([...keys]).toEqual(['$:usage.*'])
    expect(lastCount).toBe(1000)
  })

  it('normalizes structural diagnostics from a 1000-element malformed array', () => {
    const result = decodeResourceRow({ ...agent, runtime_ids: Array.from({ length: 1000 }, () => 'broken-id') })
    expect(result.value).toBeUndefined()
    expect(result.undecodable.length).toBeGreaterThan(0)
    expect([...new Set(result.undecodable.map(({ path }) => path))]).toEqual(['runtime_ids.*'])
  })

  it('does not persist a usable degraded row once it leaves server order', () => {
    const tasks: (() => void)[] = []
    const windows: ProcessedWindow<unknown>[] = []
    const ingest = makeWindowIngest({
      decode: (raw) => decodeResourceRow(raw).value,
      decodeDetailed: decodeResourceRow,
      publish: (window) => windows.push(window),
      now: () => 0,
      schedule: (work) => { tasks.push(work); return () => {} },
    })
    const good = frame([agent] as ResourceWire[])
    if (good.kind !== 'snapshot') throw new TypeError('Expected snapshot')
    // Deliberately inject a future wire case beyond the generated TypeScript contract.
    const futureRows: unknown = [{ ...agent, observation: 'future-observation' }]
    ingest.accept({
      ...good,
      items: futureRows as ResourceWire[],
    })
    while (tasks.length > 0) tasks.shift()!()
    expect(windows.at(-1)?.undecodable[0]?._tag).toBe('Degraded')
    ingest.accept({
      kind: 'changes', id: good.id, collection: good.collection,
      upserts: [], removes: [], order: [], has_more: false, snapshot: good.snapshot,
    })
    while (tasks.length > 0) tasks.shift()!()
    expect(windows.at(-1)?.items).toEqual([])
    expect(windows.at(-1)?.rawItems).toEqual([])
    expect(windows.at(-1)?.undecodable).toEqual([])
    expect(windows.at(-1)?.decodeStale).toBe(false)
  })
})
