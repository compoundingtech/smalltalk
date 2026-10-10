import type { CreateExtensions, ExtensionViews, ExtensionFixtures } from './contract.ts'
export const extensionViews: ExtensionViews = Object.freeze({})
export const extensionFixtures: ExtensionFixtures = Object.freeze({envelopes: [], subjects: []})

/** The public application has no injected renderer claims. */
export const createRendererExtensions: CreateExtensions = (_host) => Object.freeze({
  renderers: Object.freeze([]),
  native: Object.freeze([]),
})

export { createExtensions } from './public.ts'
