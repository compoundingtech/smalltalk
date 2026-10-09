import { buildSnapshotPrepare, buildSnapshotRestore, buildSnapshotSave, optionalQueueCacheSave } from './build-snapshot.ts'
import {
  defaultActionlintConfig,
  effectUtilsBinaryCaches,
  nixDevelopStep,
  plainFlakeSetupSteps,
} from '../../repos/effect-utils/genie/external.ts'

// Profiles require controls inline; namespace-features labels apply only to shape labels.
// Namespace serves queued merge-group jobs before PR, optional and manual jobs.
// This orders waiting jobs; it does not preempt active jobs or reserve a runner.
const linuxJobPriority = "${{ github.event_name == 'merge_group' && 1 || 2 }}"
export const linuxRunnerProfile = `namespace-profile-linux-x86-64;job.priority=${linuxJobPriority}`
export const macosRunnerProfile = 'namespace-profile-macos-arm64'
export const linuxRunner = [`${linuxRunnerProfile};github.run-id=\${{ github.run_id }}`] as const
/**
 * The Linux gate's stage jobs. On 2026-10-03 the shape label `nscloud-ubuntu-24.04-amd64-16x32`
 * stopped getting runners at about 12:10Z, and the profile allows only about five runners at
 * once, so with every job on it Workspace CI runs went one at a time. The 8x16 shape label still
 * got runners at once.
 */
const linuxStageShape = 'nscloud-ubuntu-24.04-amd64-8x16-with-features'
/** All non-merge Linux work retains one class; merge groups have the first queue class. */
export const linuxStageRunner = [
  `${linuxStageShape};job.priority=${linuxJobPriority}`,
  'namespace-features:github.run-id=${{ github.run_id }}',
] as const
export const macosRunner = [`${macosRunnerProfile};github.run-id=\${{ github.run_id }}`] as const
export const linuxActionlintConfig = {
  ...defaultActionlintConfig,
  selfHostedRunnerLabels: [...(defaultActionlintConfig.selfHostedRunnerLabels ?? []), ...linuxRunner, ...linuxStageRunner],
} as const

/**
 * ci1 takes the primary Workspace CI test partition when enough general runners are idle.
 * The second partition and mail canaries may each take an idle general runner; other supporting
 * jobs stay on Namespace. GitHub has no
 * overflow between runner labels, so the `pick-runner` job asks the
 * GitHub API how many ci1 runners are idle before the other jobs start, and their `runs-on` reads its
 * output. Trusted PRs labelled `ci-priority` use the reserved `ci1-priority` lane.
 * Merge groups use the local pool only when five workers are idle and two general
 * workers remain for PRs; otherwise the whole group uses Namespace. Explicit
 * CI_MERGE_CI1=on retains the incident's forced-local override.
 *
 * Off unless the repository variable `CI1_RUNNERS` is `on`: then `pick-runner` is skipped, its output
 * is empty and every workload job runs on Namespace. A pull request from a fork never runs on
 * ci1: this repository is public, and a self-hosted runner runs whatever a job asks of it.
 */
export const pickRunnerJobId = 'pick-runner'

/** One free general runner admits a primary; reserved labels stay out of that pool. */
const ci1MinIdle = 1

export const pickRunnerJob = {
  name: pickRunnerJobId,
  if: "vars.CI1_RUNNERS == 'on'",
  // GitHub-hosted, so the choice never waits for either pool it chooses between.
  'runs-on': 'ubuntu-latest',
  'timeout-minutes': 3,
  permissions: { 'pull-requests': 'read' },
  outputs: {
    ci1: '${{ steps.pick.outputs.ci1 }}',
    ci1_secondary: '${{ steps.pick.outputs.ci1_secondary }}',
    ci1_mail: '${{ steps.pick.outputs.ci1_mail }}',
    merge_ci1: '${{ steps.pick.outputs.merge_ci1 }}',
  },
  steps: [
    {
      name: 'Keep reserved primary lanes and use idle general runners for tests and canaries',
      id: 'pick',
      env: {
        // A token that may only read the organization's self-hosted runners. Forks never receive it.
        GH_TOKEN: '${{ secrets.CI1_RUNNERS_READ_TOKEN }}',
        EVENT: '${{ github.event_name }}',
        REPOSITORY: '${{ github.repository }}',
        HEAD_REPOSITORY: '${{ github.event.pull_request.head.repo.full_name }}',
        PR_LABELS: '${{ toJSON(github.event.pull_request.labels.*.name) }}',
        OWNER: '${{ github.repository_owner }}',
        MERGE_LOCAL_ONLY: '${{ vars.CI_MERGE_CI1 }}',
        NEED: `\${{ vars.CI1_MIN_IDLE || '${ci1MinIdle}' }}`,
      },
      run: `primary_label=
namespace() {
  echo "$1: Namespace"
  printf 'Unpicked runners: **Namespace** (%s)\\n' "$1" >> "$GITHUB_STEP_SUMMARY"
  exit 0
}
primary() {
  primary_label=$1
  printf 'ci1=["%s"]\\n' "$primary_label" >> "$GITHUB_OUTPUT"
  printf 'Primary runner: **ci1** (%s)\\n' "$primary_label" >> "$GITHUB_STEP_SUMMARY"
}
if [ "$EVENT" = pull_request ] && [ "$HEAD_REPOSITORY" != "$REPOSITORY" ]; then
  namespace "a pull request from a fork never runs on ci1"
fi
if [ "$EVENT" = pull_request ] && jq -e 'index("ci-priority") != null' <<< "$PR_LABELS" >/dev/null 2>&1; then
  primary ci1-priority
  echo "priority PR: reserved ci1-priority capacity"
  printf 'Runner: **ci1** (ci1-priority, ahead of ordinary PRs)\\n' >> "$GITHUB_STEP_SUMMARY"
fi
if [ "$EVENT" = merge_group ] && [ "$MERGE_LOCAL_ONLY" != on ]; then
  # Choose once for the whole group. Five simultaneous workload jobs fit locally
  # only if two general workers remain; priority-only workers are never borrowed.
  # This is an admission snapshot, not an atomic runner reservation.
  [ -n "$GH_TOKEN" ] || namespace "merge overflow: no runner status token"
  if ! runners=$(timeout 20s gh api --paginate --slurp "orgs/$OWNER/actions/runners?per_page=100" 2>/dev/null); then
    namespace "merge overflow: runner status unavailable"
  fi
  if ! capacity=$(jq -er '
    [.[].runners[] | select(.status == "online" and .busy == false)] | unique_by(.id) |
    def has($label): any(.labels[]; .name == $label);
    ([.[] | select(has("ci1-merge") and (has("ci1-priority") | not))]) as $merge |
    ([.[] | select(has("ci1") and (has("ci1-priority") | not))] | length) as $general |
    ([$merge[] | select(has("ci1") | not)] | length) as $dedicated |
    if ($merge | length) >= 5 and ($general - ([0, 5 - $dedicated] | max)) >= 2
    then "local" else "namespace" end' <<< "$runners"); then
    namespace "merge overflow: invalid runner status"
  fi
  [ "$capacity" = local ] || namespace "merge overflow: retain two PR slots or wait for local capacity"
  printf 'merge_ci1=["ci1-merge"]\\n' >> "$GITHUB_OUTPUT"
  printf 'Merge group: **ci1** (five idle workers, two general PR slots retained)\\n' >> "$GITHUB_STEP_SUMMARY"
  exit 0
fi
if [ "$EVENT" = merge_group ]; then
  # Merge groups never borrow the PR priority lane: it can have PR work ahead of them.
  primary ci1-merge
  echo "merge queue: reserved ci1-merge runners"
  printf 'Runner: **ci1** (ci1-merge, reserved merge-queue capacity)\\n' >> "$GITHUB_STEP_SUMMARY"
fi
[ -n "$GH_TOKEN" ] || namespace "no runner status token"
[[ "$NEED" =~ ^[1-9][0-9]*$ ]] || namespace "invalid minimum idle runner count"
if ! runners=$(timeout 20s gh api --paginate --slurp "orgs/$OWNER/actions/runners?per_page=100" 2>&1); then
  echo "::warning::could not list ci1's runners: $runners"
  namespace "the runner list is unavailable"
fi
if ! idle=$(jq -e '[.[].runners[] | select(.status == "online" and .busy == false and any(.labels[]; .name == "ci1"))] | length' <<< "$runners"); then
  namespace "the runner list is invalid"
fi
if [ -z "$primary_label" ]; then
  if [ "$idle" -ge "$NEED" ]; then
    primary ci1
  else
    printf 'Primary runner: **Namespace** (%s general runners idle, %s needed)\\n' "$idle" "$NEED" >> "$GITHUB_STEP_SUMMARY"
  fi
fi
# A primary on ci1 or ci1-merge may take a general worker. Account for that worker
# before offering separate slots to the second shard and canaries. A priority
# primary has its own reserved label; general workers temporarily lent to priority
# work no longer carry ci1 and therefore were excluded from the idle count above.
available=$idle
case "$primary_label" in ci1|ci1-merge) available=$((available - 1));; esac
echo "$idle general runners idle; primary minimum $NEED; extra slots $available"
for output in ci1_secondary ci1_mail; do
  if [ "$available" -gt 0 ]; then
    printf '%s=["ci1"]\\n' "$output" >> "$GITHUB_OUTPUT"
    printf '%s runner: **ci1** (idle general slot)\\n' "$output" >> "$GITHUB_STEP_SUMMARY"
    available=$((available - 1))
  else
    printf '%s runner: **Namespace** (no idle general slot)\\n' "$output" >> "$GITHUB_STEP_SUMMARY"
  fi
done`,
    },
  ],
} as const

// Merge routing applies to the whole group. The explicit local override is retained;
// otherwise the picker may admit a local group only with room left for PR checks.
const mergeCi1Labels = `github.event_name == 'merge_group' && (vars.CI_MERGE_CI1 == 'on' && '["ci1-merge"]' || needs.pick-runner.outputs.merge_ci1)`
const pickedOr = (namespaceLabels: string, output = 'ci1') =>
  `\${{ fromJSON(${mergeCi1Labels} || (github.event_name != 'merge_group' && needs.${pickRunnerJobId}.outputs.${output}) || ${namespaceLabels}) }}`

// Keep run affinity within each event class. Merge groups take the first Namespace
// queue class; optional/manual and PR work retain the same second class.
// Set this repository variable only after a runner administrator has provisioned
// the profile with the existing image/cache and left it capacity outside the PR pool.
// An unset variable retains the existing shapes with merge-first queue ordering.
const namespaceLabels = (labels: readonly string[]) =>
  `github.event_name == 'merge_group' && vars.CI_MERGE_NAMESPACE_PROFILE && format('["namespace-profile-{0};job.priority=1;github.run-id={1}"]', vars.CI_MERGE_NAMESPACE_PROFILE, github.run_id) || format('${JSON.stringify(labels).replaceAll('${{ github.run_id }}', '{0}').replaceAll(linuxJobPriority, '{1}')}', github.run_id, github.event_name == 'merge_group' && 1 || 2)`
/** `runs-on` for a stage job: picked ci1, else the shared Namespace queue class. */
export const linuxStageRunsOn = pickedOr(
  namespaceLabels(linuxStageRunner),
)

/** Extra jobs use idle general slots unless all merge work is routed to the provisioned reserve. */
export const secondaryStageRunsOn = pickedOr(
  namespaceLabels(linuxStageRunner),
  'ci1_secondary',
)
export const mailStageRunsOn = pickedOr(
  namespaceLabels(linuxStageRunner),
  'ci1_mail',
)

/** `runs-on` for a Linux profile job: picked ci1, else the shared Namespace queue class. */
export const linuxRunsOn = pickedOr(
  namespaceLabels(linuxRunner),
)

/** Supporting jobs use Namespace unless the merge-only ci1 reservation is enabled. */
export const supportingLinuxRunsOn = `\${{ fromJSON(${mergeCi1Labels} || ${namespaceLabels(linuxRunner)}) }}`
export const supportingStageRunsOn = `\${{ fromJSON(${mergeCi1Labels} || ${namespaceLabels(linuxStageRunner)}) }}`

/** A job that needs `pick-runner` still runs when it was skipped (ci1 off). */
export const afterPickRunner = { needs: [pickRunnerJobId], if: '${{ !cancelled() }}' } as const

/** The public, read-only effect-utils cache supplies genie and other pinned effect-utils packages. */
export const readOnlyBinaryCaches = Object.values(effectUtilsBinaryCaches)

/** Dev/test builds without debug information or incremental state, as on the fleet runners. */
export const buildEnv = { CARGO_PROFILE_DEV_DEBUG: '0', CARGO_PROFILE_TEST_DEBUG: '0', CARGO_INCREMENTAL: '0' }

export const cargoCacheStep = {
  name: 'Restore the Cargo target and registry',
  id: 'cargo-cache',
  if: "env.CI_LOCAL_CACHES != '1' && env.CI_BUILD_SNAPSHOT_HIT != '1'",
  uses: 'actions/cache/restore@v4',
  with: {
    path: '${{ github.workspace }}/target\n${{ runner.temp }}/cargo-home/registry\n${{ runner.temp }}/cargo-home/git',
    key: "cargo-${{ github.job }}-${{ runner.os }}-${{ hashFiles('Cargo.lock', 'flake.lock', 'Cargo.toml', 'crates/**/Cargo.toml', '.cargo/config.toml') }}",
    'restore-keys': 'cargo-${{ github.job }}-${{ runner.os }}-',
  },
} as const

export const nixCacheStep = {
  name: 'Restore the local Nix cache',
  id: 'nix-cache',
  if: "env.CI_LOCAL_CACHES != '1' && env.CI_BUILD_SNAPSHOT_HIT != '1'",
  uses: 'actions/cache/restore@v4',
  with: {
    path: '${{ runner.temp }}/st-ci-cache',
    key: "nix5-${{ github.job }}-${{ runner.os }}-${{ hashFiles('flake.lock', 'flake.nix', 'nix/**/*.nix', '.github/fleet-compat-baseline.json', '.github/messaging-compat-baseline.json') }}",
    'restore-keys': 'nix5-${{ github.job }}-${{ runner.os }}-\nnix4-${{ github.job }}-${{ runner.os }}-',
  },
} as const

/** Keep shared dependency entries on protected main; PR builds have exact-source artifacts. */
export const saveMainDependencyCaches = (setup: readonly unknown[]) => setup
  .filter((value: any) => value.id === 'cargo-cache' || value.id === 'nix-cache')
  .map((value: any) => ({
    name: `Save main ${value.id}`,
    if: `success() && github.ref == 'refs/heads/main' && (github.event_name == 'push' || github.event_name == 'workflow_dispatch' || github.event_name == 'schedule') && env.CI_LOCAL_CACHES != '1' && steps.${value.id}.outputs.cache-hit != 'true'`,
    uses: 'actions/cache/save@v4',
    with: { path: value.with.path, key: value.with.key },
  }))

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
    run: `if [ -n "\${CI_LOCAL_CARGO_HOME:-}" ]; then echo CI_LOCAL_CACHES=1 >> "$GITHUB_ENV"; fi
printf 'CARGO_HOME=%s\\nCI_CACHE_DIR=%s\\n' "\${CI_LOCAL_CARGO_HOME:-$RUNNER_TEMP/cargo-home}" "$RUNNER_TEMP/st-ci-cache" >> "$GITHUB_ENV"`,
  },
  // actions/cache is served by Namespace's accelerated cache backend and is keyed, not tied to a node.
  // Namespace cache volumes are per node and replicate in the background, so a job on another node
  // starts empty. /nix itself cannot be cached (see scripts/ci-nix-cache); RUNNER_TEMP/st-ci-cache holds a
  // local Nix binary cache instead. Linux only: the key names the job, so each stage keeps its own.
  buildSnapshotRestore,
  cargoCacheStep,
  nixCacheStep,
  buildSnapshotPrepare,
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
    if: "env.CI_LOCAL_CACHES != '1'",
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
  nixDevelopStep({ name: 'Build selected test targets first (no debug info)', command: ['bash', 'scripts/ci-nextest', 'run', '--no-run'] }),
  // The integration target already built st2 with the workspace's unified features. Running it
  // directly avoids a separate cargo run build before those features are unified.
  nixDevelopStep({ name: 'Install matching rendered hooks', command: ['bash', 'scripts/ci-install-built-hooks'] }),
]

/** Archive consumers restore tools/fixtures, never Cargo targets or another job's build snapshot. */
export const testArchiveConsumerSetup = [
  { name: "Require the shared test producer to succeed",
    env: { PRODUCER_RESULT: "${{ needs.linux-test-build.result }}" },
    run: '[ "$PRODUCER_RESULT" = success ] || { echo "::error::shared test producer failed or was skipped"; exit 1; }' },
  ...commonSetupSteps.filter((step: any) => step.id !== 'cargo-cache'
    && step !== buildSnapshotRestore && step !== buildSnapshotPrepare),
  ...testBuildSteps.slice(0, 2),
  {
    name: 'Download this run attempt’s successful test build',
    uses: 'actions/download-artifact@v4',
    with: {
      'artifact-ids': '${{ needs.linux-test-build.outputs.artifact-id }}',
      'merge-multiple': true,
      path: '${{ runner.temp }}/ci-test-archives',
    },
  },
  { ...nixDevelopStep({ name: 'Verify source, hashes and extract test archives',
    command: ['python3', 'scripts/ci-test-archive', 'consume'] }),
    env: { CI_TEST_ARCHIVE_MANIFEST_SHA256: '${{ needs.linux-test-build.outputs.manifest-sha256 }}',
      CI_TEST_ARCHIVE_PRODUCER_ATTEMPT: '${{ needs.linux-test-build.outputs.producer-attempt }}' } },
  testBuildSteps[3],
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
  env: { ...buildEnv, ...env, CI_CACHE_DEV_SHELL: 'default' },
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
      if: `success() && env.CI_LOCAL_CACHES != '1' && ${optionalQueueCacheSave}`,
      run: 'bash scripts/ci-nix-cache save || echo "::warning::could not save the local Nix cache"',
    },
    ...saveMainDependencyCaches(setup),
    ...buildSnapshotSave,
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
  if: "env.CI_BUILD_SNAPSHOT_STORES_HIT != '1'",
  uses: 'actions/cache@v4',
  with: {
    path: '${{ runner.temp }}/st-bench',
    key: `perf-${stage}-stores-\${{ hashFiles('crates/st3/tests/daemon_bench.rs', 'docs/st3/schema.md') }}`,
  },
})
