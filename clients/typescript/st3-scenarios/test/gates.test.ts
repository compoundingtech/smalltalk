import { spawnSync } from 'node:child_process'
import { cpSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

import { afterEach, describe, expect, it } from 'vitest'

import { ANCHOR_MS, catalog, loadWorld, type Slice } from '../src/index.ts'
import { decodeFixtures, decodeSlice, everyVariant } from '../scripts/decode.ts'
import { checkTree, FIXTURES_DIR, generate } from '../scripts/emit.ts'
import { checkPublicRepo, scanContent, scanIdentities, scanTree, scanVocabulary } from '../scripts/scan.ts'

const scratch: string[] = []
afterEach(() => scratch.splice(0).forEach((dir) => rmSync(dir, { recursive: true, force: true })))

describe('decode gate', () => {
  it('decodes every ordinary-world variant at two instants; huge has bounded coverage separately', () => {
    // CLI decode still exhausts all huge variants. Avoid duplicating that scale workload in Vitest.
    const failures = [ANCHOR_MS, Date.UTC(2101, 6, 4, 3, 2, 1, 7)].flatMap((now) =>
      catalog.filter(({ id }) => id !== 'huge').flatMap(({ id }) =>
        everyVariant(loadWorld(id, { now })).flatMap((slice) => decodeSlice(id, slice))))
    expect(failures).toEqual([])
  }, 30_000)

  it('covers every variant the worlds offer', () => {
    const world = loadWorld('fleet-mid-refactor', { now: ANCHOR_MS })
    expect(everyVariant(world).map((slice) => `${slice.kind}:${slice.variant}`)).toContain('sync:subscription-limit-local')
  })

  it('fails on a planted strict-invalid value', () => {
    const world = loadWorld('fleet-mid-refactor', { now: ANCHOR_MS })
    const roster = world.slices.roster
    const [first, ...rest] = roster.state.agents
    const planted: Slice<'roster'> = {
      ...roster,
      state: { ...roster.state, agents: [{ ...first!, surprise: true } as typeof first & { surprise: boolean }, ...rest] },
    }
    const failures = decodeSlice(world.id, planted)
    expect(failures.map((failure) => failure.pointer)).toEqual(['/state/agents/0'])
  })

  it('decodes every committed fixture file from disk', () => {
    expect(decodeFixtures(FIXTURES_DIR)).toEqual([])
  })

  it('fails on planted strict-invalid values in a committed file', () => {
    const dir = mkdtempSync(join(tmpdir(), 'st3-scenarios-decode-'))
    scratch.push(dir)
    cpSync(FIXTURES_DIR, dir, { recursive: true })
    const path = join(dir, 'fleet-mid-refactor/roster.json')
    const file = JSON.parse(readFileSync(path, 'utf8')) as { state: { agents: Record<string, unknown>[] } }
    file.state.agents[0]!.updated_at = 'not-an-rfc3339-timestamp'
    file.state.agents[1]!.active_work_count = 'wrong-type'
    writeFileSync(path, JSON.stringify(file))
    const failures = decodeFixtures(dir)
    expect(failures.map(({ world, slice, pointer }) => `${world}/${slice}${pointer}`)).toEqual([
      'fleet-mid-refactor/roster/state/agents/0',
      'fleet-mid-refactor/roster/state/agents/1',
    ])
  })
})

describe('freshness gate', () => {
  it('finds the committed tree up to date', () => {
    expect(checkTree(FIXTURES_DIR)).toEqual([])
  })

  it('names a stale, a missing and an extra file', () => {
    const dir = mkdtempSync(join(tmpdir(), 'st3-scenarios-fresh-'))
    scratch.push(dir)
    cpSync(FIXTURES_DIR, dir, { recursive: true })
    writeFileSync(join(dir, 'fleet-mid-refactor/roster.json'), '{}\n')
    rmSync(join(dir, 'index.json'))
    writeFileSync(join(dir, 'fleet-mid-refactor/stray.json'), '{}\n')
    expect(checkTree(dir, generate())).toEqual(
      expect.arrayContaining([
        { path: 'fleet-mid-refactor/roster.json', problem: 'stale' },
        { path: 'index.json', problem: 'missing' },
        { path: 'fleet-mid-refactor/stray.json', problem: 'extra' },
      ]),
    )
  })
})

describe('privacy gate', () => {
  // Planted values are assembled from parts so this file never carries them literally.
  it('flags a denylisted host', () => {
    expect(scanContent('x.json', `"host": "${'mb'}p2025"`).map((finding) => finding.rule)).toEqual(['internal host name'])
    for (const name of ['3', '4', '5']) expect(scanContent('x.json', `"host": "${'de'}v${name}"`).map((finding) => finding.rule)).toEqual(['internal host name'])
    expect(scanContent('x.json', `"host": "${'mb'}p2021"`).map((finding) => finding.rule)).toEqual(['internal host name'])
  })

  it('flags the real user name', () => {
    expect(scanContent('x.md', `owner: ${'Schick'}ling`).map((finding) => finding.rule)).toEqual(['real user name'])
  })

  it('flags email addresses outside the reserved example domains', () => {
    expect(scanContent('x.md', `contact ${'ada'}@${'corp'}.io`).map((finding) => finding.rule)).toEqual(['email address outside reserved domains'])
    expect(scanContent('x.md', 'contact ada@example.com, robin@team.example.org, avery@scenario.invalid')).toEqual([])
    expect(scanContent('x.ts', "import { St3Client } from '@smalltalk/st3-client'")).toEqual([])
  })

  it('flags IPv4 addresses outside the documentation ranges and loopback', () => {
    for (const address of [`${'10'}.0.0.7`, `${'100'}.64.1.2`, `${'192'}.168.1.20`, `${'8'}.8.8.8`]) {
      expect(scanContent('x.json', `"address": "${address}"`).map((finding) => finding.rule), address).toEqual(['IPv4 address outside documentation ranges'])
    }
    expect(scanContent('x.json', '"addresses": ["192.0.2.10", "198.51.100.7", "203.0.113.200", "127.0.0.1"]')).toEqual([])
  })

  it('flags URLs outside the reserved hosts', () => {
    for (const url of [`${'http'}://wiki.${'corp'}/page`, `${'https'}://${'build'}box:8443/`, `${'ws'}://${'10'}.1.2.3/socket`]) {
      expect(scanContent('x.md', `see ${url}`).map((finding) => finding.rule), url).toContain('internal URL')
    }
    expect(scanContent('x.ts', "fetch('http://scenario.invalid/v1/client/agents'); 'ws://localhost:4000'; 'https://docs.example.com/a'; 'http://example.org'; 'http://127.0.0.1:9/'")).toEqual([])
  })

  it('flags a home path but not a source path that contains users/', () => {
    expect(scanContent('x.json', `"cwd": "/${'home'}/someone/src"`)).toHaveLength(1)
    expect(scanContent('x.json', '"path": "packages/api/src/users/loadUser.ts"')).toEqual([])
  })

  it('flags identities outside the namespace', () => {
    const rules = scanIdentities('x.json', `"from": "${'person'}/mallory", "agent": "${'agent'}/release", "host": "${'host'}/a.b"`).map((finding) => finding.rule)
    expect(rules).toHaveLength(3)
    expect(scanIdentities('x.json', '"from": "person/ada", "agent": "agent/example/atlas/builder", "host": "host/harbor"')).toEqual([])
  })

  it('flags absolute dates and clock times in vocabulary', () => {
    expect(scanVocabulary('bank.ts', "  'merged on 2029-03-04',")).toHaveLength(1)
    expect(scanVocabulary('bank.ts', "  'standup at 9:30',")).toHaveLength(1)
    expect(scanVocabulary('bank.ts', "  'about an hour ago',")).toEqual([])
  })

  it('scans every published file of the package, including root-level files, but not ignored output or node_modules', () => {
    const root = mkdtempSync(join(tmpdir(), 'st3-scenarios-scan-'))
    scratch.push(root)
    expect(spawnSync('git', ['init', '-q'], { cwd: root }).status).toBe(0)
    const leak = `"host": "${'mb'}p2025"\n`
    for (const dir of ['fixtures', 'pkg/out', 'pkg/node_modules/dep', 'pkg/src/vocabulary']) mkdirSync(join(root, dir), { recursive: true })
    writeFileSync(join(root, 'pkg/.gitignore'), 'out/\n')
    writeFileSync(join(root, 'pkg/notes.md'), leak)
    writeFileSync(join(root, 'pkg/out/bundle.js'), leak)
    writeFileSync(join(root, 'pkg/node_modules/dep/index.js'), leak)
    writeFileSync(join(root, 'pkg/src/vocabulary/bank.ts'), "export const bank = ['about an hour ago']\n")
    const findings = scanTree({ fixturesDir: join(root, 'fixtures'), packageRoot: join(root, 'pkg'), vocabularyDir: join(root, 'pkg/src/vocabulary') })
    expect(findings.map((finding) => `${finding.path.split('/pkg/').at(-1)}:${finding.rule}`)).toEqual(['notes.md:internal host name'])
  })

  it('fails closed when check-public-repo cannot run or fails without a scoped finding', () => {
    expect(checkPublicRepo(['st3-scenarios-missing-check-binary'])).toEqual([expect.stringMatching(/^could not run st3-scenarios-missing-check-binary: .*ENOENT/u)])
    expect(checkPublicRepo([process.execPath, '-e', 'process.exit(3)'])).toEqual([expect.stringContaining('exited 3 without a finding')])
    expect(checkPublicRepo([process.execPath, '-e', "process.kill(process.pid, 'SIGTERM')"])).toEqual([expect.stringContaining('killed by SIGTERM')])
    expect(checkPublicRepo([process.execPath, '-e', "console.log('fixtures/scenarios/x.json: denied'); process.exit(1)"])).toEqual(['fixtures/scenarios/x.json: denied'])
    expect(checkPublicRepo([process.execPath, '-e', "console.log('public repository check passed')"])).toEqual([])
  })
})
