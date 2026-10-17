// The app's unit tests for the fractal-web lanes.
//
// APP_TEST_SET=all (the default; merge groups and manual runs) runs every test file.
// APP_TEST_SET=related (pull requests) runs only the test files whose import graph reaches a file the
// change touched, as `vitest related` resolves it across the app, the kit and the clients. Anything that
// could change a test without being imported by it, or that cannot be placed in the graph, runs them all:
// package, lock, TypeScript and Vitest configuration, the test environment, CI inputs, a deleted file, a
// data file, or a diff that cannot be read.
import { spawnSync } from 'node:child_process'
import { fileURLToPath } from 'node:url'
import { resolve } from 'node:path'

const GRAPH_ROOTS = ['apps/fractal-web/', 'packages/fractal-ui/', 'clients/typescript/']
const GRAPH_EXTENSIONS = /\.(?:[cm]?[jt]sx?|css)$/
const EVERYTHING = [
  [/(?:^|\/)package\.json$/, 'a package manifest'],
  [/^(?:pnpm-lock\.yaml|pnpm-workspace\.yaml|flake\.(?:nix|lock))$/, 'the lockfile or toolchain'],
  [/(?:^|\/)tsconfig[^/]*\.json$/, 'a TypeScript configuration'],
  [/(?:^|\/)(?:vite|vitest)\.[^/]*config\.[cm]?[jt]s$/, 'a Vite or Vitest configuration'],
  [/^apps\/fractal-web\/src\/testEnvironment\.ts$/, 'the test environment'],
  [/^apps\/fractal-web\/scripts\//, 'the app scripts the tests run through'],
  [/^(?:\.github\/|scripts\/ci-fractal-web)/, 'a CI input'],
]

// `entries` are `git diff --name-status --no-renames` rows: a status letter and a path.
export function selectTests(entries) {
  const files = []
  for (const [status, path] of entries) {
    const everything = EVERYTHING.find(([pattern]) => pattern.test(path))
    if (everything) return { mode: 'all', reason: `${path} is ${everything[1]}` }
    if (status !== 'A' && status !== 'M') return { mode: 'all', reason: `${path} was ${status === 'D' ? 'deleted' : 'changed in a way (' + status + ') the import graph cannot place'}` }
    if (!GRAPH_ROOTS.some(root => path.startsWith(root))) continue
    if (!GRAPH_EXTENSIONS.test(path)) return { mode: 'all', reason: `${path} is not a module the import graph follows` }
    files.push(path)
  }
  return { mode: 'related', files }
}

const run = (command, args, options) => spawnSync(command, args, { stdio: 'inherit', ...options })

function main() {
  const app = resolve(fileURLToPath(new URL('../..', import.meta.url)))
  const root = resolve(app, '../..')
  const all = reason => {
    console.log(`App unit tests: all (${reason})`)
    process.exit(run('pnpm', ['exec', 'vitest', 'run'], { cwd: app }).status ?? 1)
  }
  if ((process.env.APP_TEST_SET || 'all') === 'all') all('every test file runs outside pull requests')
  if (process.env.APP_TEST_SET !== 'related') throw new Error('APP_TEST_SET must be all or related')
  const base = process.env.BASE_SHA
  if (!base) all('no base commit to compare with')
  const git = (...args) => spawnSync('git', ['-C', root, ...args], { encoding: 'utf8' })
  // The checkout is shallow: fetch the base commit so two trees can be compared.
  if (git('cat-file', '-e', `${base}^{commit}`).status !== 0) git('fetch', '--no-tags', '--depth=1', 'origin', base)
  const diff = git('diff', '--name-status', '--no-renames', base, 'HEAD')
  if (diff.status !== 0) all(`the diff against ${base} cannot be read`)
  const entries = diff.stdout.split('\n').filter(Boolean).map(line => line.split('\t'))
  const selection = selectTests(entries)
  if (selection.mode === 'all') all(selection.reason)
  if (!selection.files.length) {
    console.log('App unit tests: none selected (no changed module is in the app, kit or client import graph)')
    return
  }
  console.log(`App unit tests: related to ${selection.files.length} changed file(s)\n${selection.files.map(file => `  ${file}`).join('\n')}`)
  process.exit(run('pnpm', ['exec', 'vitest', 'related', '--run', ...selection.files.map(file => resolve(root, file))], { cwd: app }).status ?? 1)
}

if (process.argv[1] === fileURLToPath(import.meta.url)) main()
