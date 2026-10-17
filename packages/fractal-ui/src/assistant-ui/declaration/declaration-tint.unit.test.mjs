import assert from 'node:assert/strict'
import { after, test } from 'node:test'
import { createServer } from 'vite'

const server = await createServer({ configFile: false, root: new URL('../../../', import.meta.url).pathname, optimizeDeps: { noDiscovery: true, include: [] }, server: { middlewareMode: true } })
after(() => server.close())
const { tokenizeDeclaration } = await server.ssrLoadModule('/src/assistant-ui/declaration/declaration-tint.ts')
const fixtures = await server.ssrLoadModule('/src/assistant-ui/declaration/declaration-fixtures.ts')

const text = lines => lines.map(line => line.map(token => token.text).join('')).join('\n')
const kinds = (lines, kind) => lines.flat().filter(token => token.kind === kind).map(token => token.text)

for (const [name, source, language] of [
  ['agent KDL', fixtures.agentDeclarationKdl, 'kdl'],
  ['mission KDL', fixtures.missionDeclarationKdl, 'kdl'],
  ['long KDL', fixtures.longDeclarationKdl, 'kdl'],
  ['structured JSON', fixtures.missionDeclarationJson, 'json'],
]) test(`${name} tokenizes losslessly, one display line per source line`, () => {
  const lines = tokenizeDeclaration(source, language)
  assert.equal(text(lines), source)
  assert.equal(lines.length, source.split('\n').length)
})

test('KDL tints node names, properties, values, keywords and redacted env values', () => {
  const lines = tokenizeDeclaration('version 2\nagent "example/a" {\n    harness "omp" { model "m"; }\n    env {\n        GH_TOKEN "<redacted>"\n    }\n    restart max=3 enabled=#true legacy=true\n}\n', 'kdl')
  assert.deepEqual(kinds(lines, 'node'), ['version', 'agent', 'harness', 'model', 'env', 'GH_TOKEN', 'restart'])
  assert.deepEqual(kinds(lines, 'string'), ['"example/a"', '"omp"', '"m"'])
  assert.deepEqual(kinds(lines, 'property'), ['max', 'enabled', 'legacy'])
  assert.deepEqual(kinds(lines, 'number'), ['2', '3'])
  assert.deepEqual(kinds(lines, 'keyword'), ['#true', 'true'])
  assert.deepEqual(kinds(lines, 'redacted'), ['"<redacted>"'])
})

test('KDL comments, slashdash, raw and multi-line strings keep their kind across lines', () => {
  const source = '// note\n/- skipped "x"\nnode #"raw "quoted""# /* block\nspans */ """\nmulti\nline\n"""\n'
  const lines = tokenizeDeclaration(source, 'kdl')
  assert.equal(text(lines), source)
  assert.deepEqual(kinds(lines, 'comment'), ['// note', '/-', '/* block', 'spans */'])
  assert.deepEqual(kinds(lines, 'node'), ['skipped', 'node'])
  assert.deepEqual(kinds(lines, 'string'), ['"x"', '#"raw "quoted""#', '"""', 'multi', 'line', '"""'])
})

test('JSON separates keys from values and keeps the redaction placeholder distinct', () => {
  const lines = tokenizeDeclaration('{\n  "env": { "A": "<redacted>" },\n  "n": -1.5e3,\n  "ok": true,\n  "s": "v"\n}', 'json')
  assert.deepEqual(kinds(lines, 'property'), ['"env"', '"A"', '"n"', '"ok"', '"s"'])
  assert.deepEqual(kinds(lines, 'string'), ['"v"'])
  assert.deepEqual(kinds(lines, 'number'), ['-1.5e3'])
  assert.deepEqual(kinds(lines, 'keyword'), ['true'])
  assert.deepEqual(kinds(lines, 'redacted'), ['"<redacted>"'])
})
