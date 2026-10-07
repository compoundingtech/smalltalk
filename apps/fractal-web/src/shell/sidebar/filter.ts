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
}
/** Matches a sidebar row against visibility, status, host and contextual search filters. */
export const matchesRow = ({
  row,
  filters,
  context,
}: {
  readonly row: SidebarRow
  readonly filters: SidebarFilters
  readonly context: string
}): boolean =>
  (!filters.needsMe || row.needsMe) &&
  (!filters.hideEnded || row.status !== 'ended') &&
  (!filters.hideRetired || row.status !== 'retired') &&
  (filters.host === '' || row.workspace.host === filters.host) &&
  (filters.statuses.length === 0 || filters.statuses.includes(row.status)) &&
  (filters.query.trim() === '' ||
    scoreTokens({
      tokens: tokenize(filters.query),
      primary: prepare(row.workspace.title),
      secondary: prepare(`${row.agent?.description ?? ''} ${row.workspace.host} ${context}`),
    }) > 0)

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
      return (right.agent?.lastActivityAt ?? 0) - (left.agent?.lastActivityAt ?? 0)
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
}: {
  readonly folders: readonly ProjectedFolder[]
  readonly rows: ReadonlyMap<string, SidebarRow>
  readonly filters: SidebarFilters
  readonly sort: SortMode
  readonly ancestor?: string
}): readonly ProjectedFolder[] =>
  folders.flatMap((folder) => {
    const context = `${ancestor} ${folder.name}`
    const children = filterFolders({
      folders: folder.folders,
      rows: rows,
      filters: filters,
      sort: sort,
      ancestor: context,
    })
    const members = sortMembers({
      members: folder.members.filter((id) =>
        matchesRow({ row: rows.get(id)!, filters: filters, context: context }),
      ),
      rows: rows,
      sort: sort,
    })
    const filtering =
      filters.query.trim() !== '' ||
      filters.needsMe ||
      filters.hideEnded ||
      filters.hideRetired ||
      filters.host !== '' ||
      filters.statuses.length > 0
    return filtering && children.length === 0 && members.length === 0
      ? []
      : [{ ...folder, folders: children, members }]
  })
