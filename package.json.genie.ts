import { packageJson } from './repos/effect-utils/genie/external.ts'

/**
 * The root pnpm workspace's members, listed explicitly: no `apps/**`, `packages/**` or
 * `clients/**` globs. `packages/fractal-ui`, `apps/fractal-web` and its nested
 * `packages/st3-sdk` join this list in the change that adds each package.
 */
export const workspaceMembers = [
  'clients/typescript/st3-client',
  'clients/typescript/st3-views',
  'clients/typescript/st3-scenarios',
  'apps/ios',
  'packages/fractal-ui',
  'apps/fractal-web',
  'apps/fractal-web/packages/st3-sdk',
  // Megarepo member (megarepo.kdl): fractal-web's diagnostics packages, resolved from source.
  // pnpm installs every member's devDependencies, so devbar's whole `workspace:^` closure joins.
  ...[
    'content-address',
    'devbar',
    'effect-distributed-lock',
    'effect-rpc-explorer',
    'effect-rpc-explorer-react',
    'effect-rpc-observer',
    'effect-rust',
    'meters',
    'otel-browser',
    'otel-contract',
    'rpc-devtools',
    'stylex-tokens',
    'utils',
    'utils-dev',
    'utils-storybook',
  ].map((name) => `repos/effect-utils/packages/@overeng/${name}` as const),
] as const

export default packageJson.aggregateFromPackages({
  packages: [],
  name: 'smalltalk-workspace',
  repoName: 'smalltalk',
  extraMembers: workspaceMembers,
})
