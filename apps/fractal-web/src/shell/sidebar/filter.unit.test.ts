import { describe, expect, it } from '@effect/vitest'
import { Effect } from 'effect'
import * as fc from 'fast-check'

import { applyOp, createFolder, emptyDoc, place, project } from '../../folders/core.mts'
import { dailyDriver } from '../fixtures/layouts.ts'
import { filterFolders, matchesRow, matchesSidebarHost, parseSidebarQuery, sortMembers, type SidebarRow } from './filter.ts'
import { defaultFilters } from './state.ts'

const row = (
  id: string,
  title: string,
  status: SidebarRow['status'],
  host: string,
  activity: number,
): SidebarRow => ({
  workspace: { id, title, host, layout: dailyDriver, unread: [], notifications: [] },
  agent: {
    ref: id,
    name: title,
    lifecycle: { _tag: 'Unknown' },
    host,
    terminal: `terminal/${id}`,
    connected: true,
    activity: 'idle',
    status: '',
    usage: { _tag: 'Unknown' }, checkout: { _tag: 'Unknown' }, workspace: { _tag: 'Unknown' },
    startedAt: { _tag: 'Unknown' }, endedAt: { _tag: 'Unknown' },
    blockedOn: { _tag: 'Unknown' }, ask: { _tag: 'Unknown' },
    lastActivityAt: { _tag: 'Known', value: activity },
  },
  status,
  needsMe: status === 'waiting',
})
const rows = new Map([
  ['a', row('a', 'Zulu compiler', 'idle', 'build-a', 100)],
  ['b', row('b', 'Alpha sidebar', 'waiting', 'laptop24', 300)],
  ['c', row('c', 'Beta transport', 'ended', 'build-a', 200)],
])
describe('sidebar view projection', () => {
  it('restores exact manual order after every derived sort without mutating source membership', () => {
    const manual = ['c', 'a', 'b']
    expect(sortMembers({ members: manual, rows: rows, sort: 'status' })).toEqual(['b', 'a', 'c'])
    expect(sortMembers({ members: manual, rows: rows, sort: 'activity' })).toEqual(['b', 'c', 'a'])
    expect(sortMembers({ members: manual, rows: rows, sort: 'name' })).toEqual(['b', 'c', 'a'])
    expect(sortMembers({ members: manual, rows: rows, sort: 'host' })).toEqual(['c', 'a', 'b'])
    expect(sortMembers({ members: manual, rows: rows, sort: 'manual' })).toEqual(['c', 'a', 'b'])
    expect(manual).toEqual(['c', 'a', 'b'])
  })
  it('prioritizes open attention even when the agent is not in waiting status', () => {
    const attentionRows = new Map(rows)
    attentionRows.set('c', { ...rows.get('c')!, needsMe: true })
    expect(sortMembers({ members: ['a', 'c', 'b'], rows: attentionRows, sort: 'status' })).toEqual([
      'b',
      'c',
      'a',
    ])
  })
  it('keeps every ancestor of fuzzy matches and applies host/status filters at arbitrary depth', () => {
    const doc = emptyDoc()
    const ops = [
      createFolder('root', 'Product', null, 'V', [1, 0, 't']),
      createFolder('child', 'Interface', 'root', 'V', [2, 0, 't']),
      createFolder('deep', 'Review', 'child', 'V', [3, 0, 't']),
      place('b', 'deep', 'V', [4, 0, 't']),
      place('a', 'child', 'V', [5, 0, 't']),
      place('c', 'root', 'V', [6, 0, 't']),
    ]
    for (const op of ops) applyOp(doc, op)
    const folders = project(doc, ['a', 'b', 'c']).folders
    const result = filterFolders({
      folders: folders,
      rows: rows,
      filters: {
        ...defaultFilters,
        query: 'asb',
        needsMe: true,
        host: 'laptop24',
        statuses: ['waiting'],
      },
      sort: 'manual',
    })
    expect(result.map((folder) => folder.name)).toEqual(['Product'])
    expect(result[0]?.members).toEqual([])
    expect(result[0]?.folders[0]?.name).toBe('Interface')
    expect(result[0]?.folders[0]?.folders[0]?.members).toEqual(['b'])
    expect(
      filterFolders({
        folders: folders,
        rows: rows,
        filters: { ...defaultFilters, query: 'asb', host: 'build-a' },
        sort: 'manual',
      }),
    ).toEqual([])
    expect(folders[0]?.members).toEqual(['c'])
  })
})

describe('sidebar machine query', () => {
  it.each([
    ['', '', []],
    ['  compiler  sidebar ', 'compiler sidebar', []],
    ['machine:dev3', '', [['dev3']]],
    ['machine:dev3,,dev5,', '', [['dev3', 'dev5']]],
    ['HOST:DEV machine:dev3', '', [['dev'], ['dev3']]],
    ['machine: host:,, compiler', 'compiler', []],
    ['compiler machine:dev3 sidebar host:DEV', 'compiler sidebar', [['dev3'], ['dev']]],
    ['other:dev3 compiler-machine:dev3', 'other:dev3 compiler-machine:dev3', []],
  ] as const)('parses %j without highlighting facets', (input, text, machines) => {
    expect(parseSidebarQuery(input)).toEqual({ text, tokens: text === '' ? [] : text.split(' '), facets: [], machines })
  })

  it('separates repo:, state: and tool: facets from free text', () => {
    expect(parseSidebarQuery('Repo:Example/Product compiler TOOL:omp state:')).toEqual({
      text: 'compiler', tokens: ['compiler'], facets: [{ field: 'repo', value: 'example/product' }, { field: 'tool', value: 'omp' }, { field: 'state', value: '' }], machines: [],
    })
  })

  it('matches free text against names and IDs, never the host', () => {
    const candidate = row('agent/compiler', 'Compiler', 'idle', 'host/dev3', 0)
    expect(matchesRow({ row: candidate, filters: { ...defaultFilters, query: 'compiler' } })).toBe(true)
    expect(matchesRow({ row: candidate, filters: { ...defaultFilters, query: 'agent/comp' } })).toBe(true)
    expect(matchesRow({ row: candidate, filters: { ...defaultFilters, query: 'dev3' } })).toBe(false)
    expect(matchesRow({ row: candidate, filters: { ...defaultFilters, query: 'machine:dev3' } })).toBe(true)
  })

  it('shares row host namespace stripping without rewriting query values', () => {
    expect(matchesSidebarHost('HOST/DEV3', parseSidebarQuery('machine:dev').machines)).toBe(true)
    expect(matchesSidebarHost('host/dev3', parseSidebarQuery('machine:host/dev3').machines)).toBe(false)
    expect(parseSidebarQuery('machine:HOST/DEV3').machines).toEqual([['host/dev3']])
  })

  const machineRows = [
    row('dev3', 'Compiler', 'idle', 'host/dev3', 0),
    row('dev5', 'Sidebar', 'idle', 'DEV5', 0),
    row('other', 'Compiler', 'idle', 'host/laptop24', 0),
  ]
  it.each([
    ['machine:dev3', '', ['dev3']],
    ['machine:dev3,dev5', '', ['dev3', 'dev5']],
    ['HOST:DEV', '', ['dev3', 'dev5']],
    ['machine:dev machine:dev3', '', ['dev3']],
    ['machine:dev3 host:dev5', '', []],
    ['machine:dev9', '', []],
    ['machine:', '', ['dev3', 'dev5', 'other']],
    ['machine:,,', '', ['dev3', 'dev5', 'other']],
    ['machine:dev3,,', '', ['dev3']],
    ['compiler machine:dev', '', ['dev3']],
    ['laptop machine:dev', '', []],
    ['machine:dev', 'host/dev3', ['dev3']],
    ['machine:dev3', 'DEV5', []],
    ['machine:dev', 'dev3', []],
  ] as const)('matches %j AND the exact menu host %j', (query, host, expected) => {
    expect(machineRows.filter((candidate) =>
      matchesRow({ row: candidate, filters: { ...defaultFilters, query, host } }),
    ).map((candidate) => candidate.workspace.id)).toEqual(expected)
  })

  it.live('case and alias preserve every generated host prefix match', () => Effect.sync(() => {
    fc.assert(fc.property(
      fc.array(fc.constantFrom('a', 'b', 'c', 'd', 'e', 'v', '3', '5'), { minLength: 1, maxLength: 20 }),
      fc.nat(),
      (letters, index) => {
        const host = letters.join('')
        const prefix = host.slice(0, 1 + index % host.length)
        const candidate = row('generated', 'Compiler', 'idle', `host/${host}`, 0)
        for (const key of ['machine', 'HOST']) {
          const query = `${key}:${prefix.toUpperCase()},,`
          expect(parseSidebarQuery(query)).toEqual({ text: '', tokens: [], facets: [], machines: [[prefix]] })
          expect(matchesRow({ row: candidate, filters: { ...defaultFilters, query } })).toBe(true)
        }
      },
    ))
  }))

  it('ignores empty facets when projecting empty folders', () => {
    const doc = emptyDoc()
    applyOp(doc, createFolder('empty', 'Empty', null, 'V', [1, 0, 't']))
    const folders = project(doc, []).folders
    expect(filterFolders({ folders, rows, filters: { ...defaultFilters, query: 'machine: host:,, ' }, sort: 'manual' })).toEqual(folders)
    expect(filterFolders({ folders, rows, filters: { ...defaultFilters, query: 'machine:dev9' }, sort: 'manual' })).toEqual([])
  })
})
