import { spawnSync } from 'node:child_process'
import { cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { changelogRegistryDirectory, checkChangelogRegistry, listChangelogEntryFiles, renderChangelogRegistry, writeChangelogRegistry } from '../../scripts/changelog-registry.mjs'
import { changelogSources, registerChangelogEntry } from '../../.changelog/mod.ts'
import { compileChangelogEntries } from './compiler.ts'

const entry = (title: string) => `import type { ChangelogEntrySource } from '../src/changelog/contract.ts'\n\nexport default {\n  title: '${title}',\n  summary: 'A synthetic entry used only by the registry merge test.',\n} satisfies ChangelogEntrySource\n`
const ignoreLine = '/apps/fractal-web/.changelog/registry.generated.ts\n'

let root = ''
beforeEach(() => {
  root = mkdtempSync(join(tmpdir(), 'fractal-changelog-merge-'))
})
afterEach(() => rmSync(root, { recursive: true, force: true }))

// A throwaway repository: Git alone decides whether the two merges conflict.
const git = (cwd: string, ...args: string[]) => {
  const env = Object.fromEntries(Object.entries(process.env).filter(([name]) => !name.startsWith('GIT_')))
  return spawnSync('git', ['-c', 'user.name=Test', '-c', 'user.email=test@example.invalid', '-c', 'commit.gpgsign=false', '-c', 'core.hooksPath=/dev/null', ...args], {
    cwd, encoding: 'utf8', env: { ...env, HOME: root, GIT_CONFIG_NOSYSTEM: '1' },
  })
}
const must = (cwd: string, ...args: string[]) => {
  const run = git(cwd, ...args)
  if (run.status !== 0) throw new Error(`git ${args.join(' ')}: ${run.stderr}${run.stdout}`)
  return run.stdout
}

/** The real `.changelog/` directory as a repository's first commit; `track` decides whether the generated file is committed. */
const seed = (track: boolean) => {
  const repo = join(root, track ? 'tracked' : 'untracked')
  const directory = join(repo, 'apps/fractal-web/.changelog')
  mkdirSync(directory, { recursive: true })
  for (const file of readdirSync(changelogRegistryDirectory)) if (file !== 'registry.generated.ts') cpSync(join(changelogRegistryDirectory, file), join(directory, file))
  if (track) writeFileSync(join(directory, 'registry.generated.ts'), renderChangelogRegistry(listChangelogEntryFiles(directory)))
  else writeFileSync(join(repo, '.gitignore'), ignoreLine)
  must(repo, 'init', '-q', '-b', 'main')
  must(repo, 'add', '-A')
  must(repo, 'commit', '-q', '-m', 'seed')
  return { repo, directory }
}

/** Two branches each add one entry (same day, so both sort to the very top) and the second is merged into the first. */
const mergeTwoBranches = (track: boolean) => {
  const { repo, directory } = seed(track)
  const addEntry = (branch: string, slug: string) => {
    must(repo, 'checkout', '-q', '-b', branch, 'main')
    writeFileSync(join(directory, `2026-10-16-${slug}.ts`), entry(`Synthetic ${slug}`))
    if (track) writeFileSync(join(directory, 'registry.generated.ts'), renderChangelogRegistry(listChangelogEntryFiles(directory)))
    must(repo, 'add', '-A')
    must(repo, 'commit', '-q', '-m', slug)
    return must(repo, 'diff', '--name-only', 'main', 'HEAD').trim().split('\n')
  }
  const left = addEntry('left', 'left-entry')
  const right = addEntry('right', 'right-entry')
  must(repo, 'checkout', '-q', 'left')
  return { directory, left, right, merge: git(repo, 'merge', '--no-edit', 'right') }
}

describe('derived changelog registry', () => {
  it('two branches that each add an entry merge without a conflict and list both entries', () => {
    const { directory, left, right, merge } = mergeTwoBranches(false)
    expect(left).toEqual(['apps/fractal-web/.changelog/2026-10-16-left-entry.ts'])
    expect(right).toEqual(['apps/fractal-web/.changelog/2026-10-16-right-entry.ts'])
    expect(merge.status, merge.stdout + merge.stderr).toBe(0)
    expect(existsSync(join(directory, 'registry.generated.ts'))).toBe(false)
    expect(writeChangelogRegistry(directory)).toEqual({ changed: true })
    const generated = readFileSync(join(directory, 'registry.generated.ts'), 'utf8')
    expect(generated).toContain('"2026-10-16-left-entry.ts"')
    expect(generated).toContain('"2026-10-16-right-entry.ts"')
    expect(checkChangelogRegistry(directory)).toEqual({ ok: true, reason: 'fresh' })
    const syntheticEntries = ['2026-10-16-right-entry.ts', '2026-10-16-left-entry.ts']
    expect(listChangelogEntryFiles(directory).filter((file) => syntheticEntries.includes(file))).toEqual(syntheticEntries)
  })

  it('control: the same two branches conflict when the generated registry is committed', () => {
    const { merge } = mergeTwoBranches(true)
    expect(merge.status).not.toBe(0)
    expect(merge.stdout).toContain('CONFLICT')
  })

  it('renders the same bytes for any directory order, one line per entry, newest first', () => {
    const files = listChangelogEntryFiles(changelogRegistryDirectory)
    const rendered = renderChangelogRegistry(files)
    expect(renderChangelogRegistry([...files].reverse())).toBe(rendered)
    expect(renderChangelogRegistry([...files.slice(3), ...files.slice(0, 3)])).toBe(rendered)
    for (const file of files) expect(rendered.split('\n').filter((line) => line.includes(file))).toHaveLength(2)
  })

  it('the generated module on disk is what regeneration produces, and registers every file once', () => {
    expect(checkChangelogRegistry()).toEqual({ ok: true, reason: 'fresh' })
    expect(changelogSources.map(({ file }) => file)).toEqual(listChangelogEntryFiles(changelogRegistryDirectory))
    expect(changelogSources.every(({ visibility }) => visibility === 'public')).toBe(true)
  })

  it('detects a missing or stale generated module and rejects files that are not entries', () => {
    const directory = join(root, 'directory')
    mkdirSync(directory)
    writeFileSync(join(directory, '2026-10-16-one-entry.ts'), entry('One'))
    expect(checkChangelogRegistry(directory)).toEqual({ ok: false, reason: 'missing' })
    writeChangelogRegistry(directory)
    writeFileSync(join(directory, '2026-10-16-two-entry.ts'), entry('Two'))
    expect(checkChangelogRegistry(directory)).toEqual({ ok: false, reason: 'stale' })
    writeFileSync(join(directory, 'notes.md'), 'not an entry')
    expect(() => listChangelogEntryFiles(directory)).toThrow('invalid-filename')
  })

  it('keeps the public strictness: an entry may declare itself private, and a public build rejects it', () => {
    const module = { default: { title: 'Synthetic', summary: 'A synthetic entry.' }, visibility: 'private' as const }
    const registration = registerChangelogEntry('2026-10-16-synthetic-private.ts', module)
    expect(registration.visibility).toBe('private')
    expect(() => compileChangelogEntries({ sources: [registration], audience: 'public' })).toThrow('private-entry-in-public-audience')
    expect(registerChangelogEntry('2026-10-16-synthetic-public.ts', { default: module.default }).visibility).toBe('public')
    expect(() => registerChangelogEntry('2026-10-16-synthetic-bad.ts', { default: module.default, visibility: 'secret' as never })).toThrow('visibility')
    expect(() => registerChangelogEntry('2026-10-16-synthetic-extra.ts', { default: module.default, extra: 1 } as never)).toThrow('unexpected exports extra')
  })
})
