import {
  effectUtilsBinaryCaches,
  namespaceRunner,
  nixDevelopStep,
  plainFlakeSetupSteps,
} from '../../repos/effect-utils/genie/external.ts'

export const linuxRunner = namespaceRunner({ profile: 'namespace-profile-linux-x86-64', runId: '${{ github.run_id }}' })
export const macosRunner = namespaceRunner({ profile: 'namespace-profile-macos-arm64', runId: '${{ github.run_id }}' })

/** The public, read-only effect-utils cache supplies genie and other pinned effect-utils packages. */
export const readOnlyBinaryCaches = Object.values(effectUtilsBinaryCaches)

/** Dev/test builds without debug information or incremental state, as on the fleet runners. */
export const buildEnv = { CARGO_PROFILE_DEV_DEBUG: '0', CARGO_PROFILE_TEST_DEBUG: '0', CARGO_INCREMENTAL: '0' }

/**
 * Checkout, Nix with the read-only effect-utils cache, isolated HOME/XDG,
 * provider fixtures, rendered hooks, and the standalone workspace test build.
 * `pull_request` checks out GitHub's merge ref: the PR head merged with the latest base.
 */
export const workspacePreparationSteps = [
  { uses: 'actions/checkout@v4', with: { 'fetch-depth': 0, 'persist-credentials': false } },
  // Namespace cache volume: the last committed cache is mounted at these paths for every run.
  // Needs a cache volume on the runner profile; the step's log says whether one is attached.
  {
    name: 'Mount Rust and Nix caches',
    id: 'cache',
    if: "runner.os == 'Linux'",
    uses: 'namespacelabs/nscloud-cache-action@v1',
    with: {
      cache: 'rust',
      path: '${{ github.workspace }}/target\n~/.cache/st-ci\n/nix',
    },
  },
  ...plainFlakeSetupSteps({ nix: { binaryCaches: readOnlyBinaryCaches } }),
  {
    name: 'Isolate test home and XDG state',
    run: `# The cached Cargo registry lives under the real HOME; tests get the isolated one.
printf 'CARGO_HOME=%s\\nCI_CLIPPY_TARGET=%s\\n' "\${CARGO_HOME:-$HOME/.cargo}" "$HOME/.cache/st-ci/clippy-target" >> "$GITHUB_ENV"
home="$RUNNER_TEMP/test-home"
mkdir -p "$home" "$home/.config" "$home/.cache" "$home/.local/state"
printf 'HOME=%s\\nXDG_CONFIG_HOME=%s/.config\\nXDG_CACHE_HOME=%s/.cache\\nXDG_STATE_HOME=%s/.local/state\\n' "$home" "$home" "$home" "$home" >> "$GITHUB_ENV"`,
  },
  {
    name: 'Prepare provider component fixtures',
    run: `system=$(nix eval --impure --raw --expr builtins.currentSystem)
components=$(nix build ".#checks.$system.provider-components" --no-link --print-out-paths --print-build-logs)
for provider in GITHUB_ISSUE GITHUB_PR PTY_STATS VISTA; do
  wasm=$(printf '%s' "$provider" | tr '[:upper:]' '[:lower:]')
  printf 'ST2_%s_COMPONENT=%s/share/st2/providers/st2_%s_component.component.wasm\\n' "$provider" "$components" "$wasm" >> "$GITHUB_ENV"
done`,
  },
  nixDevelopStep({ name: 'Install matching rendered hooks', command: ['cargo', 'run', '--locked', '-p', 'st2', '--', 'hooks', 'install'] }),
  nixDevelopStep({ name: 'Build workspace tests first (no debug info)', command: ['cargo', 'nextest', 'run', '--workspace', '--locked', '--profile', 'ci', '--no-run'] }),
]
