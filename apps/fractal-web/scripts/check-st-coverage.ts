// Compare the vendored or --schema <file> wire contract with the app's exhaustive case maps.
// CLI stdout/stderr are deliberately plain text so CI reports retain actionable differences.
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { Effect, Schema } from 'effect'
import { entryCases, kindCases, viewCases, KNOWN_VISIBILITIES } from '../src/conversation/vocabulary.ts'

export const defaultSchemaPath = fileURLToPath(new URL('../../../docs/st3/client-v0/schemas/client-v0.schema.json', import.meta.url))

export class CoverageError extends Schema.TaggedError<CoverageError>()('CoverageError', {
  message: Schema.String,
  cause: Schema.optional(Schema.Defect()),
}) {}

const at = (value: unknown, ...path: readonly (string | number)[]): unknown =>
  path.reduce<unknown>((node, key) => typeof node === 'object' && node !== null
    ? Reflect.get(node, key) : undefined, value)
const list = (value: unknown): readonly unknown[] => Array.isArray(value) ? value : []
const strings = (value: unknown): readonly string[] => {
  const values = list(value)
  return values.every(value => typeof value === 'string') ? values : []
}
const openEnum = (node: unknown, descriptionList: (text: string) => readonly string[]): readonly string[] => {
  const direct = strings(at(node, 'enum'))
  if (direct.length > 0) return direct
  for (const branch of list(at(node, 'anyOf'))) {
    const values = strings(at(branch, 'enum'))
    if (values.length > 0) return values
  }
  const description = at(node, 'description')
  return typeof description === 'string' ? descriptionList(description) : []
}

export const checkCoverage = (schema: unknown): { readonly lines: readonly string[]; readonly errors: readonly string[] } => {
  const defs = at(schema, '$defs')
  const views: string[] = []
  const fallback: string[] = []
  let unreadableViews = false
  for (const branch of list(at(defs, 'TimelineView', 'oneOf'))) {
    const ref = at(branch, '$ref')
    if (typeof ref === 'string' && ref.startsWith('#/$defs/')) {
      const type = at(defs, ref.slice('#/$defs/'.length), 'properties', 'type', 'const')
      if (typeof type === 'string') views.push(type)
      else unreadableViews = true
    } else {
      const excluded = strings(at(branch, 'properties', 'type', 'not', 'enum'))
      if (excluded.length === 0) unreadableViews = true
      fallback.push(...excluded)
    }
  }
  const rows: readonly { readonly vocabulary: string; readonly st: readonly string[]; readonly app: readonly string[] }[] = [
    { vocabulary: 'TimelineView.type', st: unreadableViews ? [] : views, app: Object.keys(viewCases) },
    { vocabulary: 'TimelineView fallback not.enum (st self-consistency)', st: fallback, app: views },
    { vocabulary: 'TimelineBlock.kind', st: openEnum(at(defs, 'TimelineBlock', 'properties', 'kind'), text =>
      (text.split(';')[0] ?? '').split(',').map(value => value.trim()).filter(value => /^[a-z_]+$/u.test(value))), app: Object.keys(kindCases) },
    { vocabulary: 'TimelineBlock.visibility', st: openEnum(at(defs, 'TimelineBlock', 'properties', 'visibility'), text =>
      (text.replace(/^UI hint:\s*/u, '').split(';')[0] ?? '').replace(/\(default\)/gu, '').split(/,|\bor\b/u).map(value => value.trim()).filter(value => /^[a-z-]+$/u.test(value))), app: KNOWN_VISIBILITIES },
    { vocabulary: 'TimelineEntry.type', st: openEnum(at(defs, 'TimelineEntry', 'properties', 'type'), () => []), app: Object.keys(entryCases) },
  ]
  const lines: string[] = []
  const errors: string[] = []
  for (const { vocabulary, st, app } of rows) {
    if (st.length === 0) {
      errors.push(`FAIL ${vocabulary}: could not read st's list (schema shape changed?)`)
      continue
    }
    const unmapped = st.filter(value => !app.includes(value))
    const stale = app.filter(value => !st.includes(value))
    if (unmapped.length === 0 && stale.length === 0) lines.push(`ok   ${vocabulary}: ${st.length} cases`)
    if (unmapped.length > 0) errors.push(`FAIL ${vocabulary}: st defines ${unmapped.map(value => `'${value}'`).join(', ')}; no matching case. Update the SDK and app case map.`)
    if (stale.length > 0) errors.push(`FAIL ${vocabulary}: the app maps ${stale.map(value => `'${value}'`).join(', ')}, which st no longer defines. Remove stale cases.`)
  }
  return { lines, errors }
}

export const runCoverage = Effect.fn('check-st-coverage')(function* (path: string = defaultSchemaPath) {
  const text = yield* Effect.try({ try: () => readFileSync(path, 'utf8'), catch: cause => new CoverageError({ message: `Cannot read schema ${path}`, cause }) })
  const schema = yield* Schema.decodeUnknownEffect(Schema.fromJsonString(Schema.Unknown))(text).pipe(
    Effect.mapError(cause => new CoverageError({ message: `Cannot decode schema ${path}`, cause })),
  )
  return checkCoverage(schema)
})

if (process.argv[1] !== undefined && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const args = process.argv.slice(2)
  if (args.length !== 0 && (args.length !== 2 || args[0] !== '--schema' || !args[1])) {
    console.error('Usage: bun scripts/check-st-coverage.ts [--schema <file>] (or node with TypeScript support)')
    process.exitCode = 1
  } else {
    Effect.runPromise(runCoverage(args[1] ?? defaultSchemaPath)).then(result => {
      for (const line of result.lines) console.log(line)
      for (const error of result.errors) console.error(error)
      process.exitCode = result.errors.length > 0 ? 1 : 0
    }).catch((error: unknown) => {
      console.error(`FAIL check-st-coverage: ${String(error)}`)
      process.exitCode = 1
    })
  }
}
