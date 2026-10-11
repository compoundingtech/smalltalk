import { execFileSync } from 'node:child_process'
import { readFileSync, writeFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { checkIndex } from './check-storybook-index.mjs'

export function publishRef(output, manifest, revision) {
  const contents = readFileSync(resolve(output, 'index.json'), 'utf8')
  const receipt = checkIndex(JSON.parse(contents), manifest, 'app')
  // Storybook 10's composition protocol probes both indexes and metadata even
  // though its static builder only emits index.json. Publish the real index at
  // both protocol endpoints, not an empty index or an error suppression.
  writeFileSync(resolve(output, 'stories.json'), contents)
  writeFileSync(resolve(output, 'metadata.json'), JSON.stringify({ title: 'Fractal App', revision, storyCount: receipt.stories }, null, 2) + '\n')
  return receipt
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const [output] = process.argv.slice(2)
  if (!output) throw new Error('Usage: node publish-storybook-ref.mjs path/to/app/storybook-static')
  const manifest = JSON.parse(readFileSync(new URL('../storybook-manifest.json', import.meta.url), 'utf8'))
  const revision = execFileSync('git', ['rev-parse', 'HEAD'], { encoding: 'utf8' }).trim()
  console.log(JSON.stringify(publishRef(resolve(output), manifest, revision)))
}
