import type { Plugin } from 'vite'
export type BundleArtifact =
  | { readonly type: 'chunk'; readonly code: string; readonly modules: Readonly<Record<string, unknown>> }
  | { readonly type: 'asset'; readonly source: string | Uint8Array }
export const extensionBuildPlugin: (input: { readonly entry: string; readonly composition: string }) => Plugin
export const assertExtensionBundle: (input: {
  readonly bundle: Readonly<Record<string, BundleArtifact>>
  readonly forbiddenModules: readonly RegExp[]
  readonly forbiddenContent: readonly string[]
  readonly requiredModules?: readonly RegExp[]
  readonly requiredContent?: readonly string[]
}) => void
