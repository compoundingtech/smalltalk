import type { ExtensionSet } from '../contract.ts'

/** Synthetic local code used only by the build-boundary integration test. */
export const createExtensions = (): ExtensionSet<string, string, string> => ({
  panes: ['compiled-extension-proof/pane'],
  hosts: ['compiled-extension-proof/host'],
  claims: ['sample/overview'],
})
