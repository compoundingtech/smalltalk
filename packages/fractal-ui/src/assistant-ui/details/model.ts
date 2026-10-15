import type { SidebarAgentRow } from '../sidebar/model'
import type { ResourceMetadata, ResourceData as TasteResource, ResourceFile as TasteFile } from '../resources/resource-model'

export const detailsStates = ['busy', 'partial', 'unknown', 'idle', 'new', 'absent', 'loading', 'error'] as const
export type DetailsState = typeof detailsStates[number]
export type DetailsValue<A> = ResourceMetadata<A>

/** Host-labelled observations; omitted facts are unreported, never inferred from status or age. */
export interface DetailsDisplayObservation {
  readonly label: string
  readonly value: string
  /** Host-supplied provenance or guard reason; never a guessed value. */
  readonly detail?: string
  /** This field's first observation is acquiring independently of loaded sibling fields. */
  readonly pending?: boolean
}

export interface DetailsOverview {
  readonly title: string | undefined
  readonly status: string | undefined
  /** Reported activity only; observation completeness never implies work. Omitted means no glyph. */
  readonly activity?: 'working' | 'waiting' | 'idle' | 'errored'
  readonly machine: string | undefined
  readonly harness: string | undefined
  readonly model: string | undefined
  /** Mission identity is separate from the agent/session title. */
  readonly mission?: string
  /** Explicit labels distinguish managed elapsed, root observed span and active estimates. */
  readonly durations?: readonly DetailsDisplayObservation[]
  /** Rendered status-since and incarnation-age strings, not session duration aliases. */
  readonly since: string | undefined
  readonly age: string | undefined
}

export const detailsSectionOrder = ['Workspace', 'Pull requests', 'CI runs', 'Reviews', 'Files touched', 'Terminals', 'Subagents', 'Missions / queue', 'Attention', 'Declared bindings', 'History'] as const
export type DetailsSectionName = typeof detailsSectionOrder[number]
export interface DetailsResource {
  readonly uri: string
  readonly card: TasteResource
  readonly glyph: string
  readonly agent?: SidebarAgentRow
  readonly file?: TasteFile
  readonly terminal?: readonly string[]
}
export interface DetailsSection {
  readonly title: DetailsSectionName
  readonly items: DetailsValue<readonly DetailsResource[]>
  /** Safe host copy for unreported, unavailable or ungranted items; known [] means observed empty. */
  readonly unavailableReason?: string
  /** First acquisition is pending, distinct from an unavailable or unreported result. */
  readonly pending?: boolean
}
