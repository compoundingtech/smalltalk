import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { resolve } from 'node:path'
import { Schema } from 'effect'

export const ManifestSchema = Schema.Struct({
  version: Schema.Literal(1),
  exceptions: Schema.Array(Schema.Struct({
    book: Schema.Literals(['kit', 'app']),
    title: Schema.String,
    classification: Schema.Literals(['explore', 'perf', 'library']),
    owner: Schema.String,
    reason: Schema.String,
  })),
})

/** Exact suite titles cover every exported story in the suite; no wildcard exemptions. */
export function checkIndex(index, rawManifest, book) {
  if (book !== 'kit' && book !== 'app') throw new Error(`Invalid book: ${book}`)
  const manifest = Schema.decodeUnknownSync(ManifestSchema, { onExcessProperty: 'error' })(rawManifest)
  if (index?.v !== 5 || typeof index.entries !== 'object' || index.entries === null) throw new Error('Expected a Storybook v5 built index')
  const exceptions = new Map()
  for (const entry of manifest.exceptions) {
    for (const field of ['title', 'owner', 'reason']) {
      if (entry[field].trim() === '') throw new Error(`Empty manifest ${field}`)
    }
    const key = `${entry.book}:${entry.title}`
    if (exceptions.has(key)) throw new Error(`Duplicate exception: ${key}`)
    exceptions.set(key, entry)
  }
  const seen = new Set()
  const titles = new Set()
  let stories = 0
  for (const entry of Object.values(index.entries)) {
    if (entry.type !== 'story' && entry.type !== 'docs') throw new Error(`Invalid index entry type: ${entry.type}`)
    const title = entry.title
    const allowed = book === 'kit' ? /^Fractal\/(Kit|Explore|Perf)\/[^/]+(?:\/[^/]+)*$/ : /^Fractal\/App\/[^/]+(?:\/[^/]+)*$/
    if (typeof title !== 'string' || !allowed.test(title)) throw new Error(`Non-conforming ${book} title: ${title}`)
    titles.add(title)
    if (entry.type === 'story') stories++
    const key = `${book}:${title}`
    const exception = exceptions.get(key)
    const classification = title.startsWith('Fractal/Explore/') ? 'explore' : title.startsWith('Fractal/Perf/') ? 'perf' : undefined
    if ((classification !== undefined || entry.tags?.includes('noncanonical')) && exception === undefined) throw new Error(`Non-canonical suite missing from manifest: ${title}`)
    if (exception !== undefined) {
      if (classification !== undefined && exception.classification !== classification) throw new Error(`Wrong exception classification: ${title}`)
      if (classification === undefined && exception.classification !== 'library') throw new Error(`Wrong exception classification: ${title}`)
      seen.add(key)
    }
  }
  if (stories === 0) throw new Error(`Empty ${book} story index`)
  for (const [key, entry] of exceptions) {
    if (entry.book === book && !seen.has(key)) throw new Error(`Stale exception: ${key}`)
  }
  return { book, stories, titles: [...titles].sort(), roots: [...new Set([...titles].map(title => title.split('/').slice(0, 2).join('/')))].sort() }
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const [book, indexPath, manifestPath = fileURLToPath(new URL('../storybook-manifest.json', import.meta.url))] = process.argv.slice(2)
  if (!indexPath) throw new Error('Usage: node check-storybook-index.mjs kit|app path/to/index.json [manifest.json]')
  console.log(JSON.stringify(checkIndex(JSON.parse(readFileSync(indexPath, 'utf8')), JSON.parse(readFileSync(manifestPath, 'utf8')), book), null, 2))
}
