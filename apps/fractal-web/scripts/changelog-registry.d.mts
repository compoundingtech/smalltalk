export const changelogRegistryDirectory: string
export const changelogRegistryModules: readonly string[]
export const listChangelogEntryFiles: (directory: string) => readonly string[]
export const renderChangelogRegistry: (files: readonly string[]) => string
export const checkChangelogRegistry: (directory?: string) => { readonly ok: boolean; readonly reason: 'fresh' | 'missing' | 'stale' }
export const writeChangelogRegistry: (directory?: string) => { readonly changed: boolean }
