import {
  effectUtilsBinaryCaches,
  namespaceRunner,
  nixDevelopStep,
  plainFlakeSetupSteps,
} from '../../repos/effect-utils/genie/external.ts'

export const linuxRunner = namespaceRunner({ profile: 'namespace-profile-linux-x86-64', runId: '${{ github.run_id }}' })
/**
 * The Linux gate's stage jobs. On 2026-10-03 the shape label `nscloud-ubuntu-24.04-amd64-16x32`
 * stopped getting runners at about 12:10Z, and the profile allows only about five runners at
 * once, so with every job on it Workspace CI runs went one at a time. The 8x16 shape label still
 * got runners at once.
 */
export const linuxStageRunner = ['nscloud-ubuntu-24.04-amd64-8x16'] as const
export const macosRunner = namespaceRunner({ profile: 'namespace-profile-macos-arm64', runId: '${{ github.run_id }}' })

/**
 * ci1, our own CI machine, takes a whole Workspace CI run when it has room for it; Namespace takes
 * every other run. GitHub has no overflow between runner labels, so the `pick-runner` job asks the
 * GitHub API how many ci1 runners are idle before the other jobs start, and their `runs-on` reads its
 * output. Merge-queue runs ask for `ci1-merge`, which a runner reserved for the queue also carries, so
 * queued merges never wait behind pull request pushes.
 *
 * Off unless the repository variable `CI1_RUNNERS` is `on`: then `pick-runner` is skipped, its output
 * is empty and every job runs on Namespace exactly as before. A pull request from a fork never runs on
 * ci1: this repository is public, and a self-hosted runner runs whatever a job asks of it.
 */
export const pickRunnerJobId = 'pick-runner'

/** Jobs a run starts at once (three stages, isolation-vm, genie-freshness).
 * typescript-client follows genie-freshness and reuses its slot. */
const ci1JobsAtOnce = 5

export const pickRunnerJob = {
  name: pickRunnerJobId,
  if: "vars.CI1_RUNNERS == 'on'",
  // GitHub-hosted, so the choice never waits for either pool it chooses between.
  'runs-on': 'ubuntu-latest',
  'timeout-minutes': 3,
  permissions: {},
  outputs: { ci1: '${{ steps.pick.outputs.ci1 }}' },
  steps: [
    {
      name: 'Pick ci1 when it has room, else Namespace',
      id: 'pick',
      env: {
        // A token that may only read the organization's self-hosted runners. Forks never receive it.
        GH_TOKEN: '${{ secrets.CI1_RUNNERS_READ_TOKEN }}',
        EVENT: '${{ github.event_name }}',
        REPOSITORY: '${{ github.repository }}',
        HEAD_REPOSITORY: '${{ github.event.pull_request.head.repo.full_name }}',
        OWNER: '${{ github.repository_owner }}',
        NEED: `\${{ vars.CI1_MIN_IDLE || '${ci1JobsAtOnce}' }}`,
      },
      run: `namespace() {
  echo "$1: Namespace"
  printf 'Runner: **Namespace** (%s)\\n' "$1" >> "$GITHUB_STEP_SUMMARY"
  exit 0
}
if [ "$EVENT" = pull_request ] && [ "$HEAD_REPOSITORY" != "$REPOSITORY" ]; then
  namespace "a pull request from a fork never runs on ci1"
fi
label=ci1
[ "$EVENT" = merge_group ] && label=ci1-merge
[ -n "$GH_TOKEN" ] || namespace "no runner status token"
[[ "$NEED" =~ ^[1-9][0-9]*$ ]] || namespace "invalid minimum idle runner count"
if ! runners=$(timeout 20s gh api --paginate --slurp "orgs/$OWNER/actions/runners?per_page=100" 2>&1); then
  echo "::warning::could not list ci1's runners: $runners"
  namespace "the runner list is unavailable"
fi
if ! idle=$(jq -e --arg label "$label" '[.[].runners[] | select(.status == "online" and .busy == false and any(.labels[]; .name == $label))] | length' <<< "$runners"); then
  namespace "the runner list is invalid"
fi
[ "$idle" -ge "$NEED" ] || namespace "$idle $label runners idle, $NEED needed"
printf 'ci1=["%s"]\\n' "$label" >> "$GITHUB_OUTPUT"
echo "$idle $label runners idle: ci1"
printf 'Runner: **ci1** (%s, %s idle)\\n' "$label" "$idle" >> "$GITHUB_STEP_SUMMARY"`,
    },
  ],
} as const

const pickedOr = (namespaceLabels: string) =>
  `\${{ fromJSON(needs.${pickRunnerJobId}.outputs.ci1 || ${namespaceLabels}) }}`

/** `runs-on` for a stage job: ci1 when picked, else the Namespace shape label. */
export const linuxStageRunsOn = pickedOr(`'${JSON.stringify(linuxStageRunner)}'`)

/** `runs-on` for the other Linux jobs: ci1 when picked, else the Namespace profile with run affinity. */
export const linuxRunsOn = pickedOr(
  `format('${JSON.stringify(namespaceRunner({ profile: linuxRunner[0], runId: '{0}' }))}', github.run_id)`,
)

/** A job that needs `pick-runner` still runs when it was skipped (ci1 off). */
export const afterPickRunner = { needs: [pickRunnerJobId], if: '${{ !cancelled() }}' } as const

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
  // ci1 keeps its own caches warm on the machine (a Cargo home and build directory per runner, a
  // shared sccache and the Nix store) and names its Cargo home in CI_LOCAL_CARGO_HOME. Restoring
  // the archives below there would only cost time.
  {
    name: 'Use the runner\'s own caches',
    run: 'if [ -n "${CI_LOCAL_CARGO_HOME:-}" ]; then echo CI_LOCAL_CACHES=1 >> "$GITHUB_ENV"; fi',
  },
  // actions/cache is served by Namespace's accelerated cache backend and is keyed, not tied to a node.
  // Namespace cache volumes are per node and replicate in the background, so a job on another node
  // starts empty. /nix itself cannot be cached (see scripts/ci-nix-cache); RUNNER_TEMP/st-ci-cache holds a
  // local Nix binary cache instead. Linux only: the key names the job, so each stage keeps its own.
  {
    name: 'Restore the Cargo target and registry',
    id: 'cargo-cache',
    if: "runner.os == 'Linux' && env.CI_LOCAL_CACHES != '1'",
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
    if: "runner.os == 'Linux' && env.CI_LOCAL_CACHES != '1'",
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
printf 'CARGO_HOME=%s\\nCI_CACHE_DIR=%s\\n' "\${CI_LOCAL_CARGO_HOME:-$RUNNER_TEMP/cargo-home}" "$RUNNER_TEMP/st-ci-cache" >> "$GITHUB_ENV"
home="$RUNNER_TEMP/test-home"
mkdir -p "$home" "$home/.config" "$home/.cache" "$home/.local/state"
printf 'HOME=%s\\nXDG_CONFIG_HOME=%s/.config\\nXDG_CACHE_HOME=%s/.cache\\nXDG_STATE_HOME=%s/.local/state\\n' "$home" "$home" "$home" "$home" >> "$GITHUB_ENV"`,
  },
  {
    name: 'Use the cached Nix outputs',
    if: "runner.os == 'Linux' && env.CI_LOCAL_CACHES != '1'",
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
