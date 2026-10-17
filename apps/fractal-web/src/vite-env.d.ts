/// <reference types="vite/client" />
declare module '*.css' {}

declare module 'virtual:build-identity' {
  import type { BuildIdentity } from '@overeng/utils/build-identity'
  /** Canonical kit identity plus producer metadata used by app telemetry. */
  export interface CliBuildIdentity extends BuildIdentity {
    readonly baseVersion: string
    readonly sourceKind: string
    readonly gitRev?: string | undefined
  }
  export const buildIdentity: CliBuildIdentity
  export const deploymentId: string | undefined
}
