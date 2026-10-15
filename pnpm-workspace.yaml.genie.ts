import { commonPnpmPolicySettings, pnpmWorkspaceYaml, projectPnpmSourceInputs } from './repos/effect-utils/genie/external.ts'
import { diagnosticsPackages, workspaceMembers } from './package.json.genie.ts'

export const diagnosticsSourceInputs = projectPnpmSourceInputs({
  packages: diagnosticsPackages,
  repoName: 'smalltalk',
})

// The shared native-build denylist names packages this lock does not contain; `ignoreScripts`
// already blocks every dependency lifecycle script.
const { allowBuilds: _sharedDenylist, ...pnpmPolicy } = commonPnpmPolicySettings

// One frozen root lock for every member. The hoisted linker keeps the flat `node_modules`
// layout Expo's Metro resolver already relies on; workspace packages are linked, not injected.
export default pnpmWorkspaceYaml.root({
  packages: diagnosticsPackages,
  repoName: 'smalltalk',
  extraMembers: workspaceMembers,
  ...pnpmPolicy,
  overrides: {
    ...pnpmPolicy.overrides,
    ...diagnosticsSourceInputs.overrides,
    // platform-node 4.0.0 declares shared ^4.0.0; the latest shared requires effect ^4.0.4.
    '@effect/platform-node>@effect/platform-node-shared': '4.0.0',
  },
  peerDependencyRules: {
    ...pnpmPolicy.peerDependencyRules,
    allowedVersions: {
      ...pnpmPolicy.peerDependencyRules?.allowedVersions,
      ...diagnosticsSourceInputs.peerDependencyRules.allowedVersions,
    },
  },
  minimumReleaseAgeExclude: ['@types/node'],
  nodeLinker: 'hoisted',
})
