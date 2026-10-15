import type { CreateExtensions, ExtensionViews, ExtensionFixtures, UseDetailsSections, UseDetailsSupplement } from './contract.ts'
import { unavailable } from '../data/source.ts'
export const extensionViews: ExtensionViews = Object.freeze({})
export const extensionFixtures: ExtensionFixtures = Object.freeze({envelopes: [], subjects: []})

/** The public application has no injected renderer claims. */
export const createRendererExtensions: CreateExtensions = (_host) => Object.freeze({
  renderers: Object.freeze([]),
  native: Object.freeze([]),
})

export { createExtensions } from './public.ts'
export { changelogSources, changelogDeployment } from '../../.changelog/mod.ts'

/** Public/native-only composition has no independently tracked descendant observations. */
export const useDetailsSupplement: UseDetailsSupplement = () =>
  unavailable({ reason: 'unsupported', detail: 'Descendant observations are not supplied by this composition.' })

export const useDetailsSections: UseDetailsSections = () => []
