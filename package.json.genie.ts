import { packageJson } from './repos/effect-utils/genie/external.ts'

/**
 * The root pnpm workspace's members, listed explicitly: no `apps/**`, `packages/**` or
 * `clients/**` globs. `apps/fractal-web`, `apps/fractal-web/packages/st3-sdk` and
 * `packages/fractal-ui` join this list in the change that adds each package.
 */
export const workspaceMembers = [
  'clients/typescript/st3-client',
  'clients/typescript/st3-views',
  'clients/typescript/st3-scenarios',
  'apps/ios',
] as const

export default packageJson.aggregateFromPackages({
  packages: [],
  name: 'smalltalk-workspace',
  repoName: 'smalltalk',
  extraMembers: workspaceMembers,
})
