import type { ResourceObservation } from '@smalltalk/st3-client/schema'
import { DateTime } from 'effect'

/** Rich observed resource page with continuation and incremental loading state. */
export interface ResourcePage {
  readonly items: readonly ResourceObservation[]
  readonly nextCursor: string | null
  readonly loadingMore?: boolean
  readonly pagingError?: string
}
/** Resource binding declared by an agent specification. */
export interface DeclaredResource {
  readonly name: string
  readonly kind: string
  readonly subject: string
  readonly reason?: string
}
/** Selects the first nonempty title, name, path or URL, falling back to resource identity. */
export const resourceTitle = (resource: ResourceObservation): string => {
  for (const key of ['title', 'name', 'path', 'url']) {
    const value = resource.facts[key]
    if (typeof value === 'string' && value.length > 0) return value
  }
  return resource.id
}
/** Reads the first string state, status or conclusion fact. */
export const resourceState = (resource: ResourceObservation): string | undefined => {
  for (const key of ['state', 'status', 'conclusion']) {
    const value = resource.facts[key]
    if (typeof value === 'string') return value
  }
  return undefined
}
/** Identifies resources whose observed state represents open or active work. */
export const isOpenResource = (resource: ResourceObservation): boolean => {
  const state = resourceState(resource)
  return (
    state !== undefined &&
    ['open', 'running', 'queued', 'in_progress', 'pending', 'active'].includes(state)
  )
}
/** Orders open resources first, then newest observations, with identity breaking ties. */
export const sortResources = (
  items: readonly ResourceObservation[],
): readonly ResourceObservation[] =>
  items.toSorted(
    (a, b) =>
      Number(isOpenResource(b)) - Number(isOpenResource(a)) ||
      DateTime.toEpochMillis(b.observed_at) - DateTime.toEpochMillis(a.observed_at) ||
      a.id.localeCompare(b.id),
  )
