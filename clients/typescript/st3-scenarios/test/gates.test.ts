import { cpSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

import { afterEach, describe, expect, it } from 'vitest'

import { ANCHOR_MS, loadWorld, type Slice } from '../src/index.ts'
import { decodeCatalog, decodeFixtures, decodeSlice, everyVariant } from '../scripts/decode.ts'
import { checkTree, FIXTURES_DIR, generate } from '../scripts/emit.ts'
import { scanContent, scanIdentities, scanVocabulary } from '../scripts/scan.ts'

const scratch: string[] = []
afterEach(() => scratch.splice(0).forEach((dir) => rmSync(dir, { recursive: true, force: true })))

describe('decode gate', () => {
  it('decodes every variant of every world strictly, at the anchor and at another now', () => {
    expect(decodeCatalog([ANCHOR_MS, Date.UTC(2101, 6, 4, 3, 2, 1, 7)])).toEqual([])
  })

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
})
