import { commonPnpmPolicySettings, pnpmWorkspaceYaml } from './repos/effect-utils/genie/external.ts'
import { workspaceMembers } from './package.json.genie.ts'

// The shared native-build denylist names packages this lock does not contain; `ignoreScripts`
// already blocks every dependency lifecycle script.
const { allowBuilds: _sharedDenylist, ...pnpmPolicy } = commonPnpmPolicySettings

// One frozen root lock for every member. The hoisted linker keeps the flat `node_modules`
// layout Expo's Metro resolver already relies on; workspace packages are linked, not injected.
export default pnpmWorkspaceYaml.root({
  packages: [],
  repoName: 'smalltalk',
  extraMembers: workspaceMembers,
  ...pnpmPolicy,
  nodeLinker: 'hoisted',
  // SDK exec scripts and effect-utils workspace members must share the same Vitest runner.
  overrides: {
    ...pnpmPolicy.overrides,
    vitest: '4.1.9',
    '@vitest/browser-playwright': '4.1.9',
  },
  // effect-utils members resolve their devDependencies through this lock, not effect-utils' own.
  peerDependencyRules: {
    ...pnpmPolicy.peerDependencyRules,
    allowedVersions: { ...pnpmPolicy.peerDependencyRules?.allowedVersions, '@effect/platform-node-shared>effect': '4.0.0' },
  },
})
