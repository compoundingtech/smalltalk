// Fails when TranscriptSkeleton's static or dynamic import graph reaches Markdown, refractor or the assistant-ui runtime.
// Walks package-relative modules; bare package specifiers are checked, not traversed.
import { readFileSync } from 'node:fs'
import { dirname, relative, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import ts from 'typescript'

const packageRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const entry = resolve(packageRoot, 'src/assistant-ui/composition/TranscriptSkeleton.tsx')
const forbidden = [
  { reason: 'Markdown', test: (specifier, file) => file?.endsWith('/composition/Markdown.tsx') === true },
  { reason: 'refractor', test: specifier => specifier === 'refractor' || specifier.startsWith('refractor/') },
  { reason: 'assistant-ui runtime', test: (specifier, file) => specifier === '@assistant-ui/react' || specifier.startsWith('@assistant-ui/') || file?.endsWith('/EmbraceRuntime.tsx') === true },
]
const resolveRelative = (from, specifier) => {
  const base = resolve(dirname(from), specifier)
  return [base, `${base}.tsx`, `${base}.ts`, `${base}/index.tsx`, `${base}/index.ts`].find(candidate => ts.sys.fileExists(candidate))
}
const visited = new Set()
const violations = []
const queue = [{ file: entry, chain: [] }]
while (queue.length > 0) {
  const { file, chain } = queue.shift()
  if (visited.has(file)) continue
  visited.add(file)
  const { importedFiles } = ts.preProcessFile(readFileSync(file, 'utf8'), true, true)
  for (const { fileName: specifier } of importedFiles) {
    const target = specifier.startsWith('.') ? resolveRelative(file, specifier) : undefined
    if (specifier.startsWith('.') && target === undefined) throw new Error(`Unresolved ${specifier} from ${relative(packageRoot, file)}`)
    const path = [...chain, relative(packageRoot, file), target === undefined ? specifier : relative(packageRoot, target)]
    for (const rule of forbidden) if (rule.test(specifier, target)) violations.push({ reason: rule.reason, path: path.join(' -> ') })
    if (target !== undefined) queue.push({ file: target, chain: path.slice(0, -1) })
  }
}
const modules = [...visited].map(file => relative(packageRoot, file))
if (violations.length > 0) {
  console.error(JSON.stringify({ pass: false, modules, violations }, null, 2))
  process.exit(1)
}
console.log(JSON.stringify({ pass: true, modules }, null, 2))
