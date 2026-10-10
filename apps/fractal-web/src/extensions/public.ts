import type { ExtensionSet } from './contract.ts'

/** The default composition has no extension hosts, panes, claims or loading side effects. */
export const createExtensions = <TPane, THost, TClaim>(): ExtensionSet<TPane, THost, TClaim> => Object.freeze({
  panes: Object.freeze([]),
  hosts: Object.freeze([]),
  claims: Object.freeze([]),
})
