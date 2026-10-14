import { describe, expect, it } from 'vitest'
import { searchAgents } from './sidebarSearch.fixtures.ts'
import type { SidebarNode } from '../folders/sidebarContract.ts'
import { defaultFilters } from '../shell/sidebar/state.ts'
import { parseSidebarQuery } from '../shell/sidebar/filter.ts'
import { liveSidebarTree } from './liveSidebarTree.ts'

const tree: readonly SidebarNode[] = [{ _tag: 'Folder', id: 'product', label: 'Product', collapsed: true, children: searchAgents.map(agent => ({ _tag: 'Agent', id: agent.ref, subject: agent.ref, label: agent.name })) }]
const project = (query = '', change: Partial<typeof defaultFilters> = {}) => liveSidebarTree({ tree, agents: searchAgents, needsYou: new Set(['agent/builder']), filters: { ...defaultFilters, query, ...change }, stale: false, now: 0 })
const ids = (nodes: readonly SidebarNode[]): string[] => nodes.flatMap(node => node._tag === 'Agent' ? [node.id] : ids(node.children))
describe('live sidebar search bindings', () => {
  it('retains the canonical tree when membership, order and collapse are unchanged', () => {
    expect(project()).toBe(tree)
    const openTree: readonly SidebarNode[] = [{ _tag: 'Folder', id: 'product', label: 'Product', collapsed: false, children: tree[0]!._tag === 'Folder' ? tree[0]!.children : [] }]
    const filter = (query: string) => liveSidebarTree({ tree: openTree, agents: searchAgents, needsYou: new Set(), filters: { ...defaultFilters, query }, stale: false, now: 0 })
    expect(filter('agent/')).toBe(openTree)
    expect(filter('agent/b')).not.toBe(openTree)
    expect(openTree[0]).toMatchObject({ collapsed: false })
  })
  it('combines facets and free text without passing facet syntax into highlights', () => {
    expect(parseSidebarQuery('repo:PRODUCT state:working tool:OMP bldr')).toEqual({ text: 'bldr', tokens: ['bldr'], facets: [{ field: 'repo', value: 'product' }, { field: 'state', value: 'working' }, { field: 'tool', value: 'omp' }], machines: [] })
    expect(ids(project('repo:product state:working tool:omp bldr'))).toEqual(['agent/builder'])
    expect(project('builder')[0]).toMatchObject({ collapsed: false })
  })
  it('matches free text only against displayed names and IDs, never descriptions or metadata', () => {
    expect(ids(project('builder'))).toEqual(['agent/builder'])
    expect(ids(project('agent/reviewer'))).toEqual(['agent/reviewer'])
    expect(ids(project('product'))).toEqual([])
    expect(ids(project('local'))).toEqual([])
    expect(ids(project('codex'))).toEqual([])
    expect(ids(project('repo:product'))).toEqual(['agent/builder', 'agent/finished'])
    expect(ids(project('tool:codex'))).toEqual(['agent/reviewer'])
  })
  it('keeps machine: and host: prefix facets out of free text', () => {
    expect(parseSidebarQuery('machine:LOC,dev3 host:lo bldr')).toEqual({ text: 'bldr', tokens: ['bldr'], facets: [], machines: [['loc', 'dev3'], ['lo']] })
    expect(ids(project('machine:loc'))).toHaveLength(3)
    expect(ids(project('host:dev3'))).toEqual([])
    expect(ids(project('machine:lo builder'))).toEqual(['agent/builder'])
  })
  it('does not infer unreported repositories from names or treat a missing facet value as a match', () => {
    expect(ids(project('repo:reviewer'))).toEqual([])
    expect(ids(project('repo:'))).toEqual([])
    expect(ids(project('tool:codex'))).toEqual(['agent/reviewer'])
  })
  it('keeps host filters and needs-me authoritative and toggles finished', () => {
    expect(ids(project('', { host: 'local' }))).toHaveLength(3)
    expect(ids(project('', { needsMe: true }))).toEqual(['agent/builder'])
    expect(ids(project('', { hideEnded: true, hideRetired: true }))).toEqual(['agent/builder', 'agent/reviewer'])
    expect(ids(project())).toHaveLength(3)
    expect(tree[0]).toMatchObject({ collapsed: true })
  })
  it('sorts visible siblings without rewriting canonical placement', () => {
    expect(ids(project('', { sort: 'name' }))).toEqual(['agent/builder', 'agent/finished', 'agent/reviewer'])
    expect(ids(tree)).toEqual(['agent/builder', 'agent/reviewer', 'agent/finished'])
  })
})
