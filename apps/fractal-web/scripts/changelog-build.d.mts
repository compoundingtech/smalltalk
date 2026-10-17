import type { ChangelogDeployment, ChangelogEntry, ChangelogManifest } from '../src/changelog/contract.ts'

export interface ChangelogRegistries { readonly public: string; readonly private: string }
export const defaultChangelogRegistries: ChangelogRegistries
export const ensureChangelogRegistries: (registries?: ChangelogRegistries) => void
export const prepareChangelogBuild: (options: {
  readonly privateBuild: boolean
  readonly deployment?: ChangelogDeployment | undefined
  readonly registries?: ChangelogRegistries
}) => Promise<{
  readonly deployment: ChangelogDeployment
  readonly entryIds: readonly string[]
  readonly privateEntries: readonly ChangelogEntry[]
  readonly privateDirectory: string | undefined
  readonly compile: (build: { readonly machineVersion: string }) => ChangelogManifest
}>
export const evaluateBuildIdentityModule: (code: string) => Promise<{ readonly machineVersion: string }>
export const assertPublicChangelogBoundary: (input: {
  readonly manifest: ChangelogManifest | undefined
  readonly privateEntries: readonly ChangelogEntry[]
  readonly privateDirectory: string | undefined
  readonly modules: readonly string[]
  readonly texts: readonly string[]
}) => void
