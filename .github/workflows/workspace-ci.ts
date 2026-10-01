import {
  effectUtilsBinaryCaches,
  namespaceRunner,
  nixDevelopStep,
  plainFlakeSetupSteps,
} from '../../repos/effect-utils/genie/external.ts'

export const linuxRunner = namespaceRunner({ profile: 'namespace-profile-linux-x86-64', runId: '${{ github.run_id }}' })
/**
 * The Linux gate's stage jobs use a bigger shape than the profile's 8x16: the test build, nextest
 * and clippy scale with the CPU count. Cost is not a constraint for this trial.
 */
export const linuxStageRunner = ['nscloud-ubuntu-24.04-amd64-16x32'] as const
export const macosRunner = namespaceRunner({ profile: 'namespace-profile-macos-arm64', runId: '${{ github.run_id }}' })

/** The public, read-only effect-utils cache supplies genie and other pinned effect-utils packages. */
export const readOnlyBinaryCaches = Object.values(effectUtilsBinaryCaches)

/** Dev/test builds without debug information or incremental state, as on the fleet runners. */
export const buildEnv = { CARGO_PROFILE_DEV_DEBUG: '0', CARGO_PROFILE_TEST_DEBUG: '0', CARGO_INCREMENTAL: '0' }

/**
 * Checkout, the Namespace cache volume, Nix with the read-only effect-utils cache, and an isolated
 * HOME/XDG. `pull_request` checks out GitHub's merge ref: the PR head merged with the latest base.
 */
export const commonSetupSteps = [
  { uses: 'actions/checkout@v4', with: { 'fetch-depth': 0, 'persist-credentials': false } },
  // actions/cache is served by Namespace's accelerated cache backend and is keyed, not tied to a node.
  // Namespace cache volumes are per node and replicate in the background, so a job on another node
  // starts empty. /nix itself cannot be cached (see scripts/ci-nix-cache); RUNNER_TEMP/st-ci-cache holds a
  // local Nix binary cache instead. Linux only: the key names the job, so each stage keeps its own.
  {
    name: 'Restore the Cargo target and registry',
    id: 'cargo-cache',
    if: "runner.os == 'Linux'",
    uses: 'actions/cache@v4',
    with: {
      path: '${{ github.workspace }}/target\n${{ runner.temp }}/cargo-home/registry\n${{ runner.temp }}/cargo-home/git',
      key: "cargo-${{ github.job }}-${{ runner.os }}-${{ hashFiles('Cargo.lock', 'flake.lock') }}",
      'restore-keys': 'cargo-${{ github.job }}-${{ runner.os }}-',
    },
  },
  {
    name: 'Restore the local Nix cache',
    id: 'nix-cache',
    if: "runner.os == 'Linux'",
    uses: 'actions/cache@v4',
    with: {
      path: '${{ runner.temp }}/st-ci-cache',
      key: "nix3-${{ github.job }}-${{ runner.os }}-${{ hashFiles('flake.lock') }}",
      'restore-keys': 'nix3-${{ github.job }}-${{ runner.os }}-',
    },
  },
  ...plainFlakeSetupSteps({ nix: { binaryCaches: readOnlyBinaryCaches } }),
  {
    name: 'Isolate test home and XDG state',
    run: `# Cargo's home and the CI cache directory live under RUNNER_TEMP, not HOME: tests get an isolated HOME,
# and actions/cache expands a leading tilde against the HOME of the step that runs it.
printf 'CARGO_HOME=%s\\nCI_CACHE_DIR=%s\\n' "$RUNNER_TEMP/cargo-home" "$RUNNER_TEMP/st-ci-cache" >> "$GITHUB_ENV"
home="$RUNNER_TEMP/test-home"
mkdir -p "$home" "$home/.config" "$home/.cache" "$home/.local/state"
printf 'HOME=%s\\nXDG_CONFIG_HOME=%s/.config\\nXDG_CACHE_HOME=%s/.cache\\nXDG_STATE_HOME=%s/.local/state\\n' "$home" "$home" "$home" "$home" >> "$GITHUB_ENV"`,
  },
  {
    name: 'Use the cached Nix outputs',
    if: "runner.os == 'Linux'",
    run: 'bash scripts/ci-nix-cache use || echo "::warning::the local Nix cache is unavailable; this run builds everything"',
  },
]

/** Provider fixtures, rendered hooks, and the standalone workspace test build. */
export const testBuildSteps = [
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

/** Everything a job that runs the workspace tests needs. */
export const workspacePreparationSteps = [...commonSetupSteps, ...testBuildSteps]
