import { prepare, scoreTokens, tokenize } from '../../command-palette/fuzzy.ts'
import type { Agent } from '../../data/source.ts'
import type { ProjectedFolder } from '../../folders/core.mts'
import type { Workspace } from '../workspaces.ts'
import type { SidebarFilters } from './state.ts'
import type { AgentStatus } from './StatusIcon.tsx'

/** User-facing labels for the sidebar's supported sort modes. */
export const sortModes = {
  manual: 'Manual order',
  status: 'Status priority',
  activity: 'Last activity',
  name: 'Name',
  host: 'Host',
} as const
/** Supported sidebar ordering modes. */
export type SortMode = keyof typeof sortModes
const priority: Record<AgentStatus, number> = {
  waiting: 0,
  working: 1,
  pending: 2,
  idle: 3,
  stale: 4,
  offline: 5,
  suspended: 6,
  unobserved: 7,
  ended: 8,
  retired: 9,
}
/** Workspace and agent state used to filter and order sidebar members. */
export interface SidebarMatchRow {
  readonly workspace: Pick<Workspace, 'title' | 'host'>
  readonly agent: Agent | undefined
  readonly status: AgentStatus
  readonly needsMe: boolean
}
/** Legacy sidebar additionally owns unread working state; matching does not require it. */
export interface SidebarRow extends SidebarMatchRow {
  readonly workspace: Workspace
}

/** Reported-fact facets; a missing or unreported fact never matches. */
export type SidebarFacetField = 'repo' | 'state' | 'tool'
/**
 * Parsed sidebar search. Free text and its lowercase tokens drive matching and highlights; facets
 * never reach highlights. Machine groups AND together while prefixes within a group OR together.
 */
export interface SidebarQuery {
  readonly text: string
  readonly tokens: readonly string[]
  readonly facets: readonly { readonly field: SidebarFacetField; readonly value: string }[]
  readonly machines: readonly (readonly string[])[]
}
/** Bare fuzzy text, repo:/state:/tool: facets and AND-ed machine:/host: facets of OR-ed lowercase prefixes. */
export const parseSidebarQuery = (query: string): SidebarQuery => {
  const terms: string[] = []
  const facets: { field: SidebarFacetField; value: string }[] = []
  const machines: string[][] = []
  for (const token of query.trim().split(/\s+/)) {
    const machine = /^(?:machine|host):(.*)$/i.exec(token)
    const facet = /^(repo|state|tool):(.*)$/i.exec(token)
    if (machine !== null) {
      const values = machine[1]!.toLowerCase().split(',').filter((value) => value !== '')
      if (values.length > 0) machines.push(values)
    } else if (facet !== null) {
      facets.push({ field: facet[1]!.toLowerCase() as SidebarFacetField, value: facet[2]!.toLowerCase() })
    } else if (token !== '') terms.push(token)
  }
  const text = terms.join(' ')
  return { text, tokens: tokenize(text), facets, machines }
}
/** Row host IDs omit their host/ namespace; query values remain literal shared-contract prefixes. */
export const matchesSidebarHost = (hostId: string, machines: SidebarQuery['machines']): boolean => {
  const host = hostId.toLowerCase().replace(/^host\//, '')
  return machines.every((values) => values.some((value) => host.startsWith(value)))
}
/** Any active view filter: membership may shrink, so empty folders are hidden and matches revealed. */
export const isSidebarFiltering = (filters: SidebarFilters, query: SidebarQuery): boolean =>
  query.text !== '' ||
  query.facets.length > 0 ||
  query.machines.length > 0 ||
  filters.needsMe ||
  filters.hideEnded ||
  filters.hideRetired ||
  filters.host !== '' ||
  filters.statuses.length > 0
const facetFact = (row: SidebarMatchRow, field: SidebarFacetField): string =>
  field === 'repo' ? row.agent?.checkout._tag === 'Known' ? row.agent.checkout.value.repository : ''
    : field === 'tool' ? row.agent?.harness ?? ''
      : [row.status, row.agent?.state, row.agent?.status, row.agent?.activity].filter(Boolean).join(' ')
/** Free text matches displayed agent names and IDs; other facts require explicit facets. */
export const matchesRow = ({
  row,
  filters,
  query = parseSidebarQuery(filters.query),
}: {
  readonly row: SidebarMatchRow
  readonly filters: SidebarFilters
  readonly query?: SidebarQuery
}): boolean => {
  if (
    (filters.needsMe && !row.needsMe) ||
    (filters.hideEnded && (row.status === 'ended' || row.agent?.state === 'done')) ||
    (filters.hideRetired && row.status === 'retired') ||
    (filters.host !== '' && row.workspace.host !== filters.host) ||
    (filters.statuses.length > 0 && !filters.statuses.includes(row.status))
  ) return false
  if (!matchesSidebarHost(row.workspace.host, query.machines)) return false
  if (!query.facets.every(({ field, value }) => value !== '' && facetFact(row, field).toLocaleLowerCase().includes(value))) return false
  return query.tokens.length === 0 ||
    scoreTokens({
      tokens: query.tokens,
      primary: prepare(row.workspace.title),
      secondary: prepare(row.agent?.ref ?? ''),
    }) > 0
}

/** Projection only: sorting never changes the CRDT's placement keys. */
export const sortMembers = ({
  members,
  rows,
  sort,
}: {
  readonly members: readonly string[]
  readonly rows: ReadonlyMap<string, SidebarMatchRow>
  readonly sort: SortMode
}): readonly string[] => {
  if (sort === 'manual') return members
  return members.toSorted((a, b) => {
    const left = rows.get(a)!,
      right = rows.get(b)!
    if (sort === 'status')
      return (
        Number(right.needsMe) - Number(left.needsMe) ||
        priority[left.status] - priority[right.status]
      )
    if (sort === 'activity')
      return (right.agent?.lastActivityAt._tag === 'Known' ? right.agent.lastActivityAt.value : 0) -
        (left.agent?.lastActivityAt._tag === 'Known' ? left.agent.lastActivityAt.value : 0)
    return sort === 'host'
      ? left.workspace.host.localeCompare(right.workspace.host)
      : left.workspace.title.localeCompare(right.workspace.title)
  })
}
/** Filters nested folders while retaining ancestors of matching members, then applies view-only ordering. */
export const filterFolders = ({
  folders,
  rows,
  filters,
  sort,
  query = parseSidebarQuery(filters.query),
}: {
  readonly folders: readonly ProjectedFolder[]
  readonly rows: ReadonlyMap<string, SidebarRow>
  readonly filters: SidebarFilters
  readonly sort: SortMode
  readonly query?: SidebarQuery
}): readonly ProjectedFolder[] =>
  folders.flatMap((folder) => {
    const children = filterFolders({
      folders: folder.folders,
      rows: rows,
      filters: filters,
      sort: sort,
      query,
    })
    const members = sortMembers({
      members: folder.members.filter((id) =>
        matchesRow({ row: rows.get(id)!, filters: filters, query }),
      ),
      rows: rows,
      sort: sort,
    })
    return isSidebarFiltering(filters, query) && children.length === 0 && members.length === 0
      ? []
      : [{ ...folder, folders: children, members }]
  })
