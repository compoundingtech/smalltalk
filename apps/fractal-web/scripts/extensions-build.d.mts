import type { Plugin } from 'vite'
export const extensionBuild: (options: { readonly entry: string; readonly privateBuild: boolean }) => Plugin
