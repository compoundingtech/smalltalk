import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { Effect, Schema } from 'effect'
import { describe, expect, it } from 'vitest'
import { checkCoverage, defaultSchemaPath, runCoverage } from '../../scripts/check-st-coverage.ts'

const JsonObject = Schema.Record(Schema.String, Schema.Unknown)
const object = Schema.decodeUnknownSync(JsonObject)

const withSchema = async (text: string, check: (path: string) => Promise<void>) => {
  const dir = mkdtempSync(join(tmpdir(), 'fractal-st-coverage-'))
  const path = join(dir, 'schema.json')
  try {
    writeFileSync(path, text)
    await check(path)
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
}

describe('st wire vocabulary drift', () => {
  it('accepts the real vendored schema', async () => {
    const result = await Effect.runPromise(runCoverage())
    expect(result.errors).toEqual([])
    expect(result.lines).toHaveLength(5)
  })

  it('accepts a schema file override with the same vocabulary', async () => {
    await withSchema(readFileSync(defaultSchemaPath, 'utf8'), async path => {
      const result = await Effect.runPromise(runCoverage(path))
      expect(result.errors).toEqual([])
      expect(result.lines).toHaveLength(5)
    })
  })

  it('rejects an overridden schema with an extra view and block kind', async () => {
    const schema = Schema.decodeUnknownSync(Schema.fromJsonString(JsonObject))(readFileSync(defaultSchemaPath, 'utf8'))
    const defs = object(schema.$defs)
    const view = object(defs.TimelineView)
    const branches = Schema.decodeUnknownSync(Schema.Array(Schema.Unknown))(view.oneOf)
    const block = object(defs.TimelineBlock)
    const properties = object(block.properties)
    const kind = object(properties.kind)
    const description = Schema.decodeUnknownSync(Schema.String)(kind.description)
    const future = {
      ...schema,
      $defs: {
        ...defs,
        TimelineView: { ...view, oneOf: [{ $ref: '#/$defs/TimelineViewFuture' }, ...branches] },
        TimelineViewFuture: { type: 'object', properties: { type: { const: 'future_view' } } },
        TimelineBlock: { ...block, properties: { ...properties, kind: { ...kind, description: description.replace(';', ', future_kind;') } } },
      },
    }
    const text = Schema.encodeSync(Schema.fromJsonString(Schema.Unknown))(future)
    await withSchema(text, async path => {
      const result = await Effect.runPromise(runCoverage(path))
      expect(result.errors).toEqual(expect.arrayContaining([
        expect.stringContaining("TimelineView.type: st defines 'future_view'"),
        expect.stringContaining("TimelineBlock.kind: st defines 'future_kind'"),
        expect.stringContaining('TimelineView fallback not.enum'),
      ]))
    })
  })

  it('fails clearly when a schema vocabulary cannot be read', () => {
    const result = checkCoverage({ $defs: {} })
    expect(result.errors).toHaveLength(5)
    expect(result.errors.every(error => error.includes("could not read st's list"))).toBe(true)
  })

  it('reports an unreadable schema override with its path', async () => {
    await withSchema('not JSON', async path => {
      await expect(Effect.runPromise(runCoverage(path))).rejects.toThrow(`Cannot decode schema ${path}`)
    })
  })
})
