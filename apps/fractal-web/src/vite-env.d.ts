/// <reference types="vite/client" />
declare module '*.css' {}

declare module 'virtual:build-identity' {
  /** Fields emitted by build identity producers; optional fields vary by build source. */
  export interface CliBuildIdentity {
    readonly baseVersion?: string
    readonly displayVersion?: string
    readonly machineVersion: string
    readonly sourceKind?: string
    readonly rev?: string | undefined
    readonly gitRev?: string | undefined
    readonly dirty?: boolean
    readonly commitTs?: number | undefined
    readonly buildTs?: number | undefined
  }
  export const buildIdentity: CliBuildIdentity
  export const deploymentId: string | undefined
}
