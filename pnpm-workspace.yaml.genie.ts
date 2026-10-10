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
  // @overeng/devbar is private and depends on stylex-tokens via `workspace:^`; point that edge
  // at the same file: source fractal-web uses.
  overrides: { '@overeng/stylex-tokens': 'file:repos/effect-utils/packages/@overeng/stylex-tokens' },
  // Until effect-utils#1727 widens the meters `effect` peer to include 4.0.0-rc.118.
  peerDependencyRules: {
    ...pnpmPolicy.peerDependencyRules,
    allowedVersions: { ...pnpmPolicy.peerDependencyRules?.allowedVersions, '@overeng/meters>effect': '4.0.0-rc.118' },
  },
})
