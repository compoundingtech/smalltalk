import { existsSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { basename, dirname, extname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import * as ts from 'typescript'
import { Schema } from 'effect'
import { describe, expect, it } from 'vitest'

const web = dirname(fileURLToPath(import.meta.url))
const kit = resolve(web, '../../../../packages/fractal-ui')
const kitPackage: { readonly exports: Readonly<Record<string, string>> } = JSON.parse(readFileSync(resolve(kit, 'package.json'), 'utf8'))

/** Eager mode preserves the startup proof. Coverage mode additionally traverses
 * literal import() edges without evaluating modules or starting liveRuntime. */
const eagerGraph = (entry: string, includeLazy = false) => {
  const files = new Set<string>()
  const dependencies = new Set<string>()
  const visit = (file: string) => {
    if (files.has(file)) return
    files.add(file)
    const source = ts.createSourceFile(file, readFileSync(file, 'utf8'), ts.ScriptTarget.Latest, true)
    const specifiers: string[] = []
    for (const statement of source.statements) {
      let specifier: string | undefined
      if (ts.isImportDeclaration(statement) && ts.isStringLiteral(statement.moduleSpecifier)) {
        const clause = statement.importClause
        if (clause?.isTypeOnly) continue
        if (clause !== undefined && clause.name === undefined && clause.namedBindings !== undefined && ts.isNamedImports(clause.namedBindings) && clause.namedBindings.elements.every(element => element.isTypeOnly)) continue
        specifier = statement.moduleSpecifier.text
      } else if (ts.isExportDeclaration(statement) && statement.moduleSpecifier !== undefined && ts.isStringLiteral(statement.moduleSpecifier)) {
        if (statement.isTypeOnly) continue
        if (statement.exportClause !== undefined && ts.isNamedExports(statement.exportClause) && statement.exportClause.elements.every(element => element.isTypeOnly)) continue
        specifier = statement.moduleSpecifier.text
      }
      if (specifier !== undefined) specifiers.push(specifier)
    }
    if (includeLazy) {
      const findLazy = (node: ts.Node): void => {
        if (ts.isCallExpression(node) && node.expression.kind === ts.SyntaxKind.ImportKeyword &&
            node.arguments[0] !== undefined && ts.isStringLiteral(node.arguments[0]))
          specifiers.push(node.arguments[0].text)
        ts.forEachChild(node, findLazy)
      }
      findLazy(source)
    }
    for (const specifier of specifiers) {
      dependencies.add(specifier)
      // Lazy terminal modules import emitted font/WASM assets, not TS surfaces.
      // Keep the established eager traversal semantics unchanged.
      if (includeLazy && (specifier.endsWith('?url') || /\.(?:woff2?|ttf|wasm)$/.test(specifier))) continue
      let local: string | undefined
      if (specifier.startsWith('.')) local = resolve(dirname(file), specifier)
      else if (specifier === '@smalltalk/fractal-ui' || specifier.startsWith('@smalltalk/fractal-ui/')) {
        const key = specifier === '@smalltalk/fractal-ui' ? '.' : `.${specifier.slice('@smalltalk/fractal-ui'.length)}`
        const target = kitPackage.exports[key]
        if (target === undefined) throw new Error(`Missing kit export ${specifier}`)
        local = resolve(kit, target)
      }
      if (local === undefined || ['.css', '.svg'].includes(extname(local))) continue
      const target = [local, `${local}.ts`, `${local}.tsx`, resolve(local, 'index.ts'), resolve(local, 'index.tsx')]
        .find(candidate => /\.(?:ts|tsx|mts)$/.test(candidate) && existsSync(candidate))
      if (target === undefined) throw new Error(`Unresolved runtime edge ${specifier} from ${file}`)
      visit(target)
    }
  }
  visit(entry)
  return { files: [...files], dependencies: [...dependencies] }
}

describe('conversation eager graph boundary', () => {
  it('keeps ConversationPane, the kit Transcript/Markdown and refractor outside the production shell graph', () => {
    const graph = eagerGraph(resolve(web, 'main.tsx'))
    // Positive witnesses prevent an empty or incorrectly resolved graph from passing.
    expect(graph.files).toContain(resolve(web, 'LiveAgentWorkspace.tsx'))
    expect(graph.files).toContain(resolve(kit, 'src/assistant-ui/shell.ts'))
    expect(graph.files.some(file => /\/(?:ConversationPane|Transcript|Markdown)\.tsx$/.test(file))).toBe(false)
    expect(graph.dependencies.some(specifier => /^(?:refractor|react-markdown)(?:\/|$)/.test(specifier))).toBe(false)
    expect(graph.dependencies).not.toContain('@assistant-ui/react')
    expect(graph.files).toContain(resolve(web, 'ConversationPaneFallback.tsx'))
  })

  it('still reaches the real heavy composition through the on-demand pane', () => {
    const graph = eagerGraph(resolve(web, 'ConversationPane.tsx'))
    expect(graph.files).toContain(resolve(kit, 'src/assistant-ui/composition/Transcript.tsx'))
    expect(graph.files).toContain(resolve(kit, 'src/assistant-ui/composition/Markdown.tsx'))
    expect(graph.files).not.toContain(resolve(kit, 'src/assistant-ui/index.ts'))
    expect(graph.files).not.toContain(resolve(kit, 'src/assistant-ui/workbench/Workbench.tsx'))
    expect(graph.files).not.toContain(resolve(kit, 'src/assistant-ui/composition/DiffPanel.tsx'))
    expect(graph.dependencies.some(specifier => /^refractor(?:\/|$)/.test(specifier))).toBe(true)
  })
})

interface AppStoryException {
  readonly book: string
  readonly title: string
  readonly classification: string
  readonly owner: string
  readonly reason: string
}

/** Resolve the primary component identity from imports/meta and require an
 * actual JSX mount. A local clone resolves to the story itself; React.lazy
 * resolves its literal import target, never to a misleading local alias. */
const storySurface = (file: string) => {
  const source = ts.createSourceFile(file, readFileSync(file, 'utf8'), ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX)
  const imports = new Map<string, string>()
  const variables = new Map<string, ts.Expression>()
  const mounted = new Set<string>()
  let title: string | undefined
  let component: ts.Expression | undefined
  const collect = (node: ts.Node): void => {
    if (ts.isImportDeclaration(node) && ts.isStringLiteral(node.moduleSpecifier) && !node.importClause?.isTypeOnly) {
      const target = node.moduleSpecifier.text
      const clause = node.importClause
      if (clause?.name !== undefined) imports.set(clause.name.text, target)
      if (clause?.namedBindings !== undefined && ts.isNamedImports(clause.namedBindings))
        for (const binding of clause.namedBindings.elements)
          if (!binding.isTypeOnly) imports.set(binding.name.text, target)
    }
    if (ts.isVariableDeclaration(node) && ts.isIdentifier(node.name) && node.initializer !== undefined)
      variables.set(node.name.text, node.initializer)
    if (ts.isPropertyAssignment(node) && ts.isIdentifier(node.name)) {
      if (node.name.text === 'title' && ts.isStringLiteral(node.initializer) && node.initializer.text.startsWith('Fractal/App/'))
        title = node.initializer.text
      if (node.name.text === 'component') component = node.initializer
    }
    if ((ts.isJsxOpeningElement(node) || ts.isJsxSelfClosingElement(node)) && ts.isIdentifier(node.tagName))
      mounted.add(node.tagName.text)
    ts.forEachChild(node, collect)
  }
  collect(source)
  if (title === undefined) return undefined
  if (component === undefined || !ts.isIdentifier(component) || !mounted.has(component.text))
    throw new Error(`${title}: meta.component must identify a mounted component`)
  const identity = (name: string, seen = new Set<string>()): string => {
    if (seen.has(name)) throw new Error(`${title}: cyclic component alias ${name}`)
    seen.add(name)
    const imported = imports.get(name)
    if (imported !== undefined) {
      if (!imported.startsWith('.')) throw new Error(`${title}: App component must resolve to a production module`)
      return resolve(dirname(file), imported)
    }
    const initializer = variables.get(name)
    if (initializer !== undefined && ts.isIdentifier(initializer)) return identity(initializer.text, seen)
    if (initializer !== undefined && ts.isCallExpression(initializer) &&
        ts.isPropertyAccessExpression(initializer.expression) && initializer.expression.name.text === 'lazy') {
      const targets: string[] = []
      const findImport = (node: ts.Node): void => {
        if (ts.isCallExpression(node) && node.expression.kind === ts.SyntaxKind.ImportKeyword &&
            node.arguments[0] !== undefined && ts.isStringLiteral(node.arguments[0]))
          targets.push(resolve(dirname(file), node.arguments[0].text))
        ts.forEachChild(node, findImport)
      }
      findImport(initializer)
      if (targets.length !== 1) throw new Error(`${title}: lazy component needs exactly one literal module target`)
      return targets[0]!
    }
    return file
  }
  return { title, module: identity(component.text) }
}

/** Bidirectional module coverage for the decided App boundaries in this slice.
 * The lazy-inclusive production graph proves on-demand panes are reachable;
 * the inventory does not turn every kit leaf into an App suite requirement. */
const assertAppStoryCoverage = ({
  entry, stories, required, exceptions = [],
}: {
  readonly entry: string
  readonly stories: readonly string[]
  readonly required: readonly string[]
  readonly exceptions?: readonly AppStoryException[]
}) => {
  for (const row of exceptions)
    if (row.book === 'app' && (!row.classification || !row.owner || !row.reason))
      throw new Error(`Incomplete App exception: ${row.title}`)
  const production = new Set(eagerGraph(entry, true).files)
  for (const file of required)
    if (!production.has(file)) throw new Error(`Required App surface is not production-mounted: ${file}`)
  const storied = new Set<string>()
  for (const file of stories) {
    const surface = storySurface(file)
    if (surface === undefined) continue
    const exception = exceptions.find(row => row.book === 'app' && row.title === surface.title)
    if (exception !== undefined) {
      if (!exception.classification || !exception.owner || !exception.reason)
        throw new Error(`Incomplete App exception: ${surface.title}`)
      continue
    }
    if (!production.has(surface.module))
      throw new Error(`Storied App surface is not production-mounted: ${surface.title} -> ${surface.module}`)
    storied.add(surface.module)
  }
  for (const file of required) {
    const waived = exceptions.some(row => row.book === 'app' && row.title === `Fractal/App/${basename(file, '.tsx')}`)
    if (!storied.has(file) && !waived) throw new Error(`Production App surface has no canonical story: ${file}`)
  }
}

describe('lazy-inclusive canonical App story identity', () => {
  it('covers the required production surfaces in both directions without changing the eager proof', () => {
    const manifest = resolve(kit, 'storybook-manifest.json')
    const exceptions = existsSync(manifest)
      ? Schema.decodeUnknownSync(Schema.fromJsonString(Schema.Struct({
          version: Schema.Literal(1),
          exceptions: Schema.Array(Schema.Struct({
            book: Schema.String, title: Schema.String, classification: Schema.String,
            owner: Schema.String, reason: Schema.String,
          })),
        })))(readFileSync(manifest, 'utf8')).exceptions
      : []
    const appSrc = resolve(web, '..')
    const stories = readdirSync(appSrc, { recursive: true, encoding: 'utf8' }).filter(file => file.endsWith('.stories.tsx')).map(file => resolve(appSrc, file))
    const required = [resolve(web, 'LiveAgentWorkspace.tsx'), resolve(web, 'ConversationPane.tsx')]
    assertAppStoryCoverage({ entry: resolve(web, 'main.tsx'), stories, required, exceptions })
    expect(eagerGraph(resolve(web, 'main.tsx'), true).files).toContain(resolve(web, 'ConversationPane.tsx'))
  })

  it.each(['local', 'lazy'] as const)('rejects a planted %s ConversationPane copy', mode => {
    const dir = mkdtempSync(join(tmpdir(), 'fractal-app-identity-'))
    try {
      const pane = join(dir, 'ConversationPane.tsx')
      const entry = join(dir, 'App.tsx')
      const story = join(dir, 'ConversationPane.stories.tsx')
      writeFileSync(pane, 'export const ConversationPane = () => <div data-testid="transcript-scroll" />')
      writeFileSync(entry, "import { ConversationPane } from './ConversationPane.tsx'; export const App = () => <ConversationPane />")
      const canonical = "import { ConversationPane } from './ConversationPane.tsx'; const meta = { title: 'Fractal/App/ConversationPane', component: ConversationPane, render: () => <ConversationPane /> }; export default meta"
      writeFileSync(story, canonical)
      const args = { entry, stories: [story], required: [pane] }
      expect(() => assertAppStoryCoverage(args)).not.toThrow()
      if (mode === 'local')
        writeFileSync(story, canonical.replace("import { ConversationPane } from './ConversationPane.tsx';", 'const ConversationPane = () => <div data-testid="transcript-scroll" />;'))
      else {
        writeFileSync(join(dir, 'ConversationPaneCopy.tsx'), 'export const ConversationPane = () => <div data-testid="transcript-scroll" />')
        writeFileSync(story, canonical.replace("import { ConversationPane } from './ConversationPane.tsx';", "import * as React from 'react'; const ConversationPane = React.lazy(() => import('./ConversationPaneCopy.tsx').then(module => ({ default: module.ConversationPane })));"))
      }
      expect(() => assertAppStoryCoverage(args)).toThrow('Storied App surface is not production-mounted')
      writeFileSync(story, canonical)
      expect(() => assertAppStoryCoverage({ ...args, stories: [] })).toThrow('Production App surface has no canonical story')
    } finally { rmSync(dir, { recursive: true, force: true }) }
  })
})
