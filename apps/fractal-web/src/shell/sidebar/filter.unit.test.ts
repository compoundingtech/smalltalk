import { describe, expect, it } from '@effect/vitest'
import { Effect } from 'effect'
import * as fc from 'fast-check'

import { applyOp, createFolder, emptyDoc, place, project } from '../../folders/core.mts'
import { dailyDriver } from '../fixtures/layouts.ts'
import { filterFolders, matchesRow, matchesShow, matchesSidebarHost, parseSidebarQuery, printSidebarQuery, sortMembers, type SidebarRow } from './filter.ts'
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
  asking: status === 'waiting',
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
  it('keeps every ancestor of fuzzy matches and applies Show/status filters at arbitrary depth', () => {
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
        query: 'is:asking machine:laptop asb',
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
        filters: { ...defaultFilters, query: 'asb machine:build-a' },
        sort: 'manual',
      }),
    ).toEqual([])
    expect(folders[0]?.members).toEqual(['c'])
  })
})

describe('sidebar machine query', () => {
  it.each([
    ['', '', false, []],
    ['  compiler  sidebar ', 'compiler sidebar', false, []],
    ['machine:host-a', '', false, [['host-a']]],
    ['machine:host-a,,host-b,', '', false, [['host-a', 'host-b']]],
    ['HOST:HOST machine:host-a', '', false, [['host'], ['host-a']]],
    ['machine: host:,, compiler', 'compiler', false, []],
    ['compiler machine:host-a sidebar host:HOST', 'compiler sidebar', false, [['host-a'], ['host']]],
    ['other:host-a compiler-machine:host-a', 'other:host-a compiler-machine:host-a', false, []],
    ['IS:Asking compiler', 'compiler', true, []],
    ['is:asking is:asking machine:host-a', '', true, [['host-a']]],
    ['is:askingx is:', 'is:askingx is:', false, []],
  ] as const)('parses %j without highlighting facets', (input, query, asking, machines) => {
    expect(parseSidebarQuery(input)).toEqual({ query, asking, machines })
  })

  it('shares row host namespace stripping without rewriting query values', () => {
    expect(matchesSidebarHost('HOST/SYNTHETIC-138/1', parseSidebarQuery('machine:synthetic-138').machines)).toBe(true)
    expect(matchesSidebarHost('host/synthetic-138/1', parseSidebarQuery('machine:host/synthetic-138/1').machines)).toBe(false)
    expect(parseSidebarQuery('machine:HOST/SYNTHETIC-138/1').machines).toEqual([['host/synthetic-138/1']])
  })

  const machineRows = [
    { ...row('host-a', 'Compiler', 'idle', 'host/host-a', 0), asking: true },
    row('host-b', 'Sidebar', 'idle', 'HOST-B', 0),
    row('other', 'Compiler', 'idle', 'host/laptop24', 0),
  ]
  it.each([
    ['machine:host-a', ['host-a']],
    ['machine:host-a,host-b', ['host-a', 'host-b']],
    ['HOST:HOST', ['host-a', 'host-b']],
    ['machine:host machine:host-a', ['host-a']],
    ['machine:host-a host:host-b', []],
    ['machine:host-z', []],
    ['machine:', ['host-a', 'host-b', 'other']],
    ['machine:,,', ['host-a', 'host-b', 'other']],
    ['machine:host-a,,', ['host-a']],
    ['compiler machine:host', ['host-a']],
    ['laptop machine:host', []],
    ['is:asking', ['host-a']],
    ['is:asking machine:host-a,host-b', ['host-a']],
    ['is:asking machine:host-b', []],
    ['is:asking machine:host-b,laptop', []],
  ] as const)('matches %j: Show filters AND, machines OR within a token', (query, expected) => {
    expect(machineRows.filter((candidate) =>
      matchesRow({ row: candidate, filters: { ...defaultFilters, query }, context: '' }),
    ).map((candidate) => candidate.workspace.id)).toEqual(expected)
  })

  it('requires the asking signal itself, not a waiting status (negative control)', () => {
    const parsedQuery = parseSidebarQuery('is:asking')
    expect(matchesShow({ host: 'host-a', asking: false, parsedQuery })).toBe(false)
    expect(matchesRow({ row: { ...row('w', 'Waiting', 'waiting', 'host-a', 0), asking: false }, filters: { ...defaultFilters, query: 'is:asking' }, context: '' })).toBe(false)
    expect(matchesShow({ host: 'host-a', asking: true, parsedQuery })).toBe(true)
  })

  it.live('printing and reparsing any query is stable', () => Effect.sync(() => {
    const word = fc.stringMatching(/^[a-z0-9-]{1,8}$/u)
    fc.assert(fc.property(
      fc.boolean(),
      fc.array(fc.array(word, { minLength: 1, maxLength: 3 }), { maxLength: 3 }),
      fc.array(word, { maxLength: 3 }),
      (asking, machines, words) => {
        const parsed = { query: words.join(' '), asking, machines }
        const printed = printSidebarQuery(parsed)
        expect(parseSidebarQuery(printed)).toEqual(parsed)
        expect(printSidebarQuery(parseSidebarQuery(printed))).toBe(printed)
      },
    ))
  }))

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
          expect(parseSidebarQuery(query)).toEqual({ query: '', asking: false, machines: [[prefix]] })
          expect(matchesRow({ row: candidate, filters: { ...defaultFilters, query }, context: '' })).toBe(true)
        }
      },
    ))
  }))

  it('ignores empty facets when projecting empty folders', () => {
    const doc = emptyDoc()
    applyOp(doc, createFolder('empty', 'Empty', null, 'V', [1, 0, 't']))
    const folders = project(doc, []).folders
    expect(filterFolders({ folders, rows, filters: { ...defaultFilters, query: 'machine: host:,, ' }, sort: 'manual' })).toEqual(folders)
    expect(filterFolders({ folders, rows, filters: { ...defaultFilters, query: 'machine:host-z' }, sort: 'manual' })).toEqual([])
  })
})
