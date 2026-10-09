import { existsSync, readFileSync } from 'node:fs'
import { dirname, extname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import * as ts from 'typescript'
import { describe, expect, it } from 'vitest'

const web = dirname(fileURLToPath(import.meta.url))
const kit = resolve(web, '../../../../packages/fractal-ui')
const kitPackage: { readonly exports: Readonly<Record<string, string>> } = JSON.parse(readFileSync(resolve(kit, 'package.json'), 'utf8'))

/** Traverse runtime import/export edges, not erased types or asynchronous import calls.
 * Third-party leaves are recorded rather than evaluated: this guard must never load the kit. */
const eagerGraph = (entry: string) => {
  const files = new Set<string>()
  const dependencies = new Set<string>()
  const visit = (file: string) => {
    if (files.has(file)) return
    files.add(file)
    const source = ts.createSourceFile(file, readFileSync(file, 'utf8'), ts.ScriptTarget.Latest, true)
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
      if (specifier === undefined) continue
      dependencies.add(specifier)
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
