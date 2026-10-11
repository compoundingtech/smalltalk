import { describe, expect, it } from 'vitest'

import { applyOp, createFolder, emptyDoc, place, project } from '../../folders/core.mts'
import { dailyDriver } from '../fixtures/layouts.ts'
import { filterFolders, sortMembers, type SidebarRow } from './filter.ts'
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
