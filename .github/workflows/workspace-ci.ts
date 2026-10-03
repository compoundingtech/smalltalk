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
/**
 * The cost check's shape: its generation and counting use one core. A shape label rather than the
 * profile, whose jobs queue behind the profile's own concurrent-runner limit.
 */
export const perfCostRunner = ['nscloud-ubuntu-24.04-amd64-8x16'] as const
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
      key: "nix4-${{ github.job }}-${{ runner.os }}-${{ hashFiles('flake.lock', '.github/fleet-compat-baseline.json', '.github/messaging-compat-baseline.json') }}",
      'restore-keys': 'nix4-${{ github.job }}-${{ runner.os }}-',
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
    name: 'Prepare the historical messaging channel',
    if: "runner.os == 'Linux'",
    run: `binary=$(timeout 10m bash scripts/messaging-compat-binary)
printf 'ST3_MESSAGING_COMPAT_BIN=%s\\n' "$binary" >> "$GITHUB_ENV"`,
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
  nixDevelopStep({ name: 'Build selected test targets first (no debug info)', command: ['bash', 'scripts/ci-nextest', 'run', '--no-run'] }),
]

/** Everything a job that runs the workspace tests needs. */
export const workspacePreparationSteps = [...commonSetupSteps, ...testBuildSteps]

// One Linux stage: its own runner and caches, the common setup, then scripts/ci-linux (or the
// command given), with steps before and after it.
export const linuxStageJob = ({
  name,
  stage,
  setup,
  description,
  env = {},
  extraLogs = '',
  command = ['bash', 'scripts/ci-linux', stage],
  before = [],
  after = [],
  runsOn = linuxStageRunner,
  condition,
}: {
  name: string
  stage: string
  setup: readonly unknown[]
  description?: string
  env?: Record<string, string>
  extraLogs?: string
  command?: string[]
  before?: readonly unknown[]
  after?: readonly unknown[]
  runsOn?: unknown
  condition?: string
}) => ({
  name,
  ...(condition ? { if: condition } : {}),
  'runs-on': runsOn,
  'timeout-minutes': 120,
  defaults: { run: { shell: 'bash' } },
  env: { ...buildEnv, ...env },
  steps: [
    ...setup,
    {
      name: 'Summarize tested revision',
      run: `printf 'Checked merge/commit: \\x60%s\\x60 on %s CPUs, %s\\n\\n| Stage | Result | Elapsed | Exit |\\n| --- | --- | --- | --- |\\n' "$(git rev-parse HEAD)" "$(nproc)" "$(free -h | awk '/^Mem:/ {print $2 " memory"}')" >> "$GITHUB_STEP_SUMMARY"`,
    },
    ...before,
    nixDevelopStep({ name: description ?? 'Run nextest', command }),
    ...after,
    {
      name: 'Save Nix outputs to the local Nix cache',
      if: 'success()',
      run: 'bash scripts/ci-nix-cache save || echo "::warning::could not save the local Nix cache"',
    },
    {
      name: 'Retain stage logs and timings',
      uses: 'actions/upload-artifact@v4',
      if: 'always()',
      with: {
        name: `${name}-logs`,
        path: `\${{ runner.temp }}/ci-logs/\n${extraLogs}`,
        'if-no-files-found': 'ignore',
      },
    },
  ],
})

/**
 * The perf jobs' generated stores, kept for as long as the generator and the schema stay the
 * same: a store from an older generator must never be measured.
 */
export const perfStoresCache = (stage: string) => ({
  name: 'Restore the generated stores',
  uses: 'actions/cache@v4',
  with: {
    path: '${{ runner.temp }}/st-bench',
    key: `perf-${stage}-stores-\${{ hashFiles('crates/st3/tests/daemon_bench.rs', 'docs/st3/schema.md') }}`,
  },
})
