import type { ExtensionRegistry, RegistryInput } from './contract.ts'

/** Snapshot once at application initialization. Later array mutation cannot add a plugin. */
export const createExtensionRegistry = <TPane, THost, TClaim>({
  publicClaims, extensions, validateClaims,
}: RegistryInput<TPane, THost, TClaim>): ExtensionRegistry<TPane, THost, TClaim> => {
  const owned = Object.freeze([...publicClaims])
  const injected = Object.freeze([...extensions.claims])
  const claims = Object.freeze([...owned, ...injected])
  validateClaims(claims)
  return Object.freeze({
    panes: Object.freeze([...extensions.panes]),
    hosts: Object.freeze([...extensions.hosts]),
    publicClaims: owned,
    extensionClaims: injected,
    claims,
  })
}
