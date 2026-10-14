import type { Agent } from '../data/source.ts'
import type { SidebarNode } from '../folders/sidebarContract.ts'
import { isSidebarFiltering, matchesRow, parseSidebarQuery, sortMembers, type SidebarMatchRow } from '../shell/sidebar/filter.ts'
import type { SidebarFilters } from '../shell/sidebar/state.ts'
import { sidebarRow } from './sidebarRow.ts'

/** View-only filtering keeps the canonical arrangement and its move fences untouched. */
export const liveSidebarTree = ({ tree, agents, needsYou, filters, stale, now }: {
  readonly tree: readonly SidebarNode[]
  readonly agents: readonly Agent[]
  readonly needsYou: ReadonlySet<string>
  readonly filters: SidebarFilters
  readonly stale: boolean
  readonly now: number
}): readonly SidebarNode[] => {
  const query = parseSidebarQuery(filters.query)
  const rows = new Map<string, SidebarMatchRow>(agents.map(agent => [agent.ref, { workspace: { title: agent.name, host: agent.host }, agent, status: sidebarRow({ agent, stale, now }).status, needsMe: needsYou.has(agent.ref) }]))
  const filtering = isSidebarFiltering(filters, query)
  const visit = (nodes: readonly SidebarNode[]): readonly SidebarNode[] => {
    const visible = nodes.flatMap((node): SidebarNode[] => {
      if (node._tag === 'Agent') {
        const row = rows.get(node.id)
        return row !== undefined && matchesRow({ row, filters, query }) ? [node] : []
      }
      const children = visit(node.children)
      const collapsed = filtering ? false : node.collapsed
      return filtering && children.length === 0 ? [] : [children === node.children && collapsed === node.collapsed ? node : { ...node, children, collapsed }]
    })
    if (filters.sort === 'manual') return visible.length === nodes.length && visible.every((node, index) => node === nodes[index]) ? nodes : visible
    const byId = new Map(visible.map(node => [node.id, node]))
    const sorted = sortMembers({ members: visible.filter(node => node._tag === 'Agent').map(node => node.id), rows, sort: filters.sort })
    return [...visible.filter(node => node._tag !== 'Agent'), ...sorted.map(id => byId.get(id)!)]
  }
  return visit(tree)
}
