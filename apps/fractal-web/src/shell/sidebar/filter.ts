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
export interface SidebarRow {
  readonly workspace: Workspace
  readonly agent: Agent | undefined
  readonly status: AgentStatus
  readonly needsMe: boolean
  /** st reports an ask-tool question or open attention for this agent; see data/asking.ts. */
  readonly asking: boolean
}

/** Parsed sidebar search: `is:asking`, AND-ed machine groups of OR-ed prefixes, and bare text. */
export interface SidebarQuery {
  readonly query: string
  readonly asking: boolean
  readonly machines: readonly (readonly string[])[]
}
/** The Show menu's "Waiting for answer" filter, as a search token. */
export const askingToken = 'is:asking'
/** Bare fuzzy text, `is:asking`, and AND-ed machine facets, each containing OR-ed lowercase prefixes. */
export const parseSidebarQuery = (query: string): SidebarQuery => {
  const terms: string[] = []
  const machines: string[][] = []
  let asking = false
  for (const token of query.trim().split(/\s+/)) {
    const facet = /^(?:machine|host):(.*)$/i.exec(token)
    if (token.toLowerCase() === askingToken) asking = true
    else if (facet === null) {
      if (token !== '') terms.push(token)
    } else {
      const values = facet[1]!.toLowerCase().split(',').filter((value) => value !== '')
      if (values.length > 0) machines.push(values)
    }
  }
  return { query: terms.join(' '), asking, machines }
}
/** Canonical search text for a parsed query; parsing the result returns the same query. */
export const printSidebarQuery = ({ query, asking, machines }: SidebarQuery): string =>
  [...(asking ? [askingToken] : []), ...machines.map((values) => `machine:${values.join(',')}`), query]
    .filter((part) => part !== '')
    .join(' ')
/** The machine value a row host ID matches against: lowercase, without its host/ namespace. */
export const sidebarHostValue = (hostId: string): string => hostId.toLowerCase().replace(/^host\//, '')
/** Row host IDs omit their host/ namespace; query values remain literal shared-contract prefixes. */
export const matchesSidebarHost = (hostId: string, machines: SidebarQuery['machines']): boolean => {
  const host = sidebarHostValue(hostId)
  return machines.every((values) => values.some((value) => host.startsWith(value)))
}
/** Show menu filters: every active filter applies (AND); machines OR within one token. */
export const matchesShow = ({
  host,
  asking,
  parsedQuery,
}: {
  readonly host: string
  readonly asking: boolean
  readonly parsedQuery: SidebarQuery
}): boolean => (!parsedQuery.asking || asking) && matchesSidebarHost(host, parsedQuery.machines)
/** Matches a sidebar row against visibility, status, Show and contextual search filters. */
export const matchesRow = ({
  row,
  filters,
  context,
  parsedQuery = parseSidebarQuery(filters.query),
}: {
  readonly row: SidebarRow
  readonly filters: SidebarFilters
  readonly context: string
  readonly parsedQuery?: SidebarQuery
}): boolean => {
  if (
    (filters.hideEnded && row.status === 'ended') ||
    (filters.hideRetired && row.status === 'retired') ||
    (filters.statuses.length > 0 && !filters.statuses.includes(row.status))
  ) return false
  if (!matchesShow({ host: row.workspace.host, asking: row.asking, parsedQuery })) return false
  return parsedQuery.query === '' ||
    scoreTokens({
      tokens: tokenize(parsedQuery.query),
      primary: prepare(row.workspace.title),
      secondary: prepare(`${row.agent?.description ?? ''} ${row.workspace.host} ${context}`),
    }) > 0
}

/** Projection only: sorting never changes the CRDT's placement keys. */
export const sortMembers = ({
  members,
  rows,
  sort,
}: {
  readonly members: readonly string[]
  readonly rows: ReadonlyMap<string, SidebarRow>
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
  ancestor = '',
  parsedQuery = parseSidebarQuery(filters.query),
}: {
  readonly folders: readonly ProjectedFolder[]
  readonly rows: ReadonlyMap<string, SidebarRow>
  readonly filters: SidebarFilters
  readonly sort: SortMode
  readonly ancestor?: string
  readonly parsedQuery?: SidebarQuery
}): readonly ProjectedFolder[] =>
  folders.flatMap((folder) => {
    const context = `${ancestor} ${folder.name}`
    const children = filterFolders({
      folders: folder.folders,
      rows: rows,
      filters: filters,
      sort: sort,
      ancestor: context,
      parsedQuery,
    })
    const members = sortMembers({
      members: folder.members.filter((id) =>
        matchesRow({ row: rows.get(id)!, filters: filters, context: context, parsedQuery }),
      ),
      rows: rows,
      sort: sort,
    })
    const filtering =
      parsedQuery.query !== '' ||
      parsedQuery.asking ||
      parsedQuery.machines.length > 0 ||
      filters.hideEnded ||
      filters.hideRetired ||
      filters.statuses.length > 0
    return filtering && children.length === 0 && members.length === 0
      ? []
      : [{ ...folder, folders: children, members }]
  })
