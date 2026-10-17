import { Schema } from 'effect'
import * as Atom from 'effect/reactivity/Atom'

import { persistedAtom } from '../../state/persistence.ts'

/** Persisted sidebar search, visibility, status and ordering settings; Show filters live in the query. */
export const SidebarFilters = Schema.Struct({
  query: Schema.String,
  hideEnded: Schema.Boolean,
  hideRetired: Schema.Boolean,
  statuses: Schema.Array(Schema.String),
  sort: Schema.Literals(['manual', 'status', 'activity', 'name', 'host']),
})
export type SidebarFilters = typeof SidebarFilters.Type
/** Unfiltered manual-order sidebar settings for a new gateway. */
export const defaultFilters: SidebarFilters = {
  query: '',
  hideEnded: false,
  hideRetired: false,
  statuses: [],
  sort: 'manual',
}
const filters = Atom.family((gateway: string) =>
  persistedAtom({
    key: `${gateway}:sidebar.filters`,
    schema: SidebarFilters,
    defaultValue: defaultFilters,
  }),
)
const collapsed = Atom.family((gateway: string) =>
  persistedAtom({
    key: `${gateway}:sidebar.collapsedFolders`,
    schema: Schema.Array(Schema.String),
    defaultValue: [],
  }),
)
/** Gateway-scoped persisted sidebar filters and collapsed-folder state. */
export const sidebarState = (gateway: string) => ({
  filters: filters(gateway),
  collapsed: collapsed(gateway),
})
/** Cache the subscribed atoms themselves: a weak-cached wrapper could be collected between renders. */
const fixtureFilters = Atom.family((_gateway: string) =>
  Atom.make(defaultFilters).pipe(Atom.keepAlive),
)
const fixtureCollapsed = Atom.family((_gateway: string) =>
  Atom.make<readonly string[]>([]).pipe(Atom.keepAlive),
)
/** Story registries are isolated, including filters and collapse; no writes to live browser state. */
export const fixtureSidebarState = (gateway: string) => ({
  filters: fixtureFilters(gateway),
  collapsed: fixtureCollapsed(gateway),
})
