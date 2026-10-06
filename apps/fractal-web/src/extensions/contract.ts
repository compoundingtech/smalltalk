/** A composition supplies bindings; the application retains its routing and claim vocabulary. */
export interface ExtensionSet<TPane, THost, TClaim> {
  readonly panes: readonly TPane[]
  readonly hosts: readonly THost[]
  readonly claims: readonly TClaim[]
}

export interface ExtensionRegistry<TPane, THost, TClaim> {
  readonly panes: readonly TPane[]
  readonly hosts: readonly THost[]
  readonly publicClaims: readonly TClaim[]
  readonly extensionClaims: readonly TClaim[]
  readonly claims: readonly TClaim[]
}

/** Hosts decide overlap semantics once, including overlaps between both ownership groups. */
export interface RegistryInput<TPane, THost, TClaim> {
  readonly publicClaims: readonly TClaim[]
  readonly extensions: ExtensionSet<TPane, THost, TClaim>
  readonly validateClaims: (claims: readonly TClaim[]) => void
}
