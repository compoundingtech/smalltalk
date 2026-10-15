import { readFileSync } from 'node:fs'
import { buildSnapshotPrepare, buildSnapshotRestore, buildSnapshotSave, optionalQueueCacheSave } from './build-snapshot.ts'
import {
  defaultActionlintConfig,
  effectUtilsBinaryCaches,
  nixDevelopStep,
  plainFlakeSetupSteps,
} from '../../repos/effect-utils/genie/external.ts'

// Profiles require controls inline; namespace-features labels apply only to shape labels.
// Merge groups and releases precede required PR checks; optional work follows both.
// This orders waiting jobs; it does not preempt active jobs or reserve a runner.
const linuxJobPriority = "${{ github.event_name == 'merge_group' && 1 || 3 }}"
export const linuxRunnerProfile = `namespace-profile-linux-x86-64;job.priority=${linuxJobPriority}`
const releaseJobPriority = "${{ github.event_name == 'pull_request' && 2 || 1 }}"
export const releaseLinuxRunnerProfile = `namespace-profile-linux-x86-64;job.priority=${releaseJobPriority}`
export const releaseMacosRunnerProfile = `namespace-profile-macos-arm64;job.priority=${releaseJobPriority}`
export const releaseLinuxRunner = "${{ fromJSON(needs['pick-runner'].outputs.ci1 || '[\"ci1-priority\"]') }}"
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
// Optional load benchmarks follow required PR checks, merge groups and releases.
export const performanceRunner = [
  `${linuxStageShape};job.priority=3`,
  'namespace-features:github.run-id=${{ github.run_id }}',
] as const
export const macosRunner = [`${macosRunnerProfile};github.run-id=\${{ github.run_id }}`] as const
export const linuxActionlintConfig = {
  ...defaultActionlintConfig,
  selfHostedRunnerLabels: [...(defaultActionlintConfig.selfHostedRunnerLabels ?? []), ...linuxRunner, ...linuxStageRunner, 'ci1-priority', 'ci2', 'ci2-merge', 'ci2-priority'],
} as const

/** Merge groups use the shared owned priority role; the scheduler chooses the host.
 * Trusted PRs may use distinct idle general workers in group3 or the ci-priority lane.
 * Fork PR code never reaches a self-hosted runner. CI1_RUNNERS controls PR picking only.
 */
export const pickRunnerJobId = 'pick-runner'

/** One free general runner admits a primary; reserved labels stay out of that pool. */
const ci1MinIdle = 1

export const pickRunnerJob = {
  name: pickRunnerJobId,
  if: "github.event_name == 'merge_group' || vars.CI1_RUNNERS == 'on'",
  // GitHub-hosted, so the choice never waits for either pool it chooses between.
  'runs-on': 'ubuntu-latest',
  'timeout-minutes': 3,
  permissions: { 'pull-requests': 'read' },
  outputs: {
    ci1: '${{ steps.pick.outputs.ci1 }}',
    ci1_secondary: '${{ steps.pick.outputs.ci1_secondary }}',
    ci1_mail: '${{ steps.pick.outputs.ci1_mail }}',
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
        NEED: `\${{ vars.CI1_MIN_IDLE || '${ci1MinIdle}' }}`,
        MERGE_OVERFLOW: '${{ vars.CI_MERGE_OVERFLOW }}',
        MERGE_NEED: "${{ vars.CI_MERGE_MIN_IDLE || '3' }}",
        RUN_ID: '${{ github.run_id }}',
      },
      run: `python3 - <<'CI_PICK_RUNNER'\n${readFileSync(new URL('../../scripts/ci-pick-runner', import.meta.url), 'utf8')}CI_PICK_RUNNER\n`,
    },
  ],
} as const

/** Releases share the pool choice, without the ordinary PR opt-in variable. */
export const priorityRunnerJob = {
  ...pickRunnerJob,
  if: "github.event_name != 'pull_request'",
  steps: pickRunnerJob.steps.map(step => ({ ...step, env: { ...step.env, MODE: 'priority' } })),
} as const

// Every required merge job follows one pool decision, including explicit overflow.
const mergeCi1Labels = `github.event_name == 'merge_group' && (needs.${pickRunnerJobId}.outputs.ci1 || '["ci1-priority"]')`
const pickedOr = (namespaceLabels: string, output = 'ci1') =>
  `\${{ fromJSON(${mergeCi1Labels} || (github.event_name != 'merge_group' && needs.${pickRunnerJobId}.outputs.${output}) || ${namespaceLabels}) }}`

// Deployment and merge progress take precedence during saturation. Required PR
// checks precede optional/manual work; local reservations remain unchanged.
const workspaceJobPriority = "github.event_name == 'merge_group' && 1 || github.event_name == 'pull_request' && 2 || 3"
// Set this repository variable only after a runner administrator has provisioned
// the profile with the existing image/cache and left it capacity outside the PR pool.
// An unset variable retains the existing shapes and Workspace priority policy.
const namespaceLabels = (labels: readonly string[]) =>
  `github.event_name == 'merge_group' && vars.CI_MERGE_NAMESPACE_PROFILE && format('["namespace-profile-{0};job.priority=1;github.run-id={1}"]', vars.CI_MERGE_NAMESPACE_PROFILE, github.run_id) || format('${JSON.stringify(labels).replaceAll('${{ github.run_id }}', '{0}').replaceAll(linuxJobPriority, '{1}')}', github.run_id, ${workspaceJobPriority})`
/** `runs-on` for a stage job: picked ci1, else the shared Namespace queue class. */
export const linuxStageRunsOn = pickedOr(
  namespaceLabels(linuxStageRunner),
)

/** Both test consumers and their producer use the owned merge lane. */
export const primaryTestRunsOn = pickedOr(namespaceLabels(linuxStageRunner), 'ci1')

/** Extra PR jobs use idle general slots; merges always use the priority reserve. */
export const secondaryStageRunsOn = pickedOr(
  namespaceLabels(linuxStageRunner),
  'ci1_secondary',
)
export const mailStageRunsOn = pickedOr(
  namespaceLabels(linuxStageRunner),
  'ci1_mail',
)

// Extra merge partitions use either host's merge role; an overflowed group stays
// wholly on its selected Namespace profile rather than mixing source cohorts.
export const extraMergeTestRunsOn = `\${{ fromJSON(needs.${pickRunnerJobId}.outputs.ci1 == '["ci1-priority"]' && '["ci1-merge"]' || needs.${pickRunnerJobId}.outputs.ci1 || '["ci1-merge"]') }}`

/** `runs-on` for a Linux profile job: picked ci1, else the shared Namespace queue class. */
export const linuxRunsOn = pickedOr(
  namespaceLabels(linuxRunner),
)

/** Supporting merge jobs use the priority lane; supporting PR jobs use Namespace. */
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
  // The integration targets already built the driver library's matching hook installer.
  // Run it directly without the legacy runner CLI or a separate cargo run build.
  nixDevelopStep({ name: 'Install matching rendered hooks', command: ['bash', 'scripts/ci-install-built-hooks'] }),
]

/** Archive consumers restore tools/fixtures, never Cargo targets or another job's build snapshot. */
export const testArchiveConsumerSetup = [
  { name: "Require the shared test producer to succeed",
    env: { PRODUCER_RESULT: "${{ needs.linux-test-build.result }}" },
    run: '[ "$PRODUCER_RESULT" = success ] || { echo "::error::shared test producer failed or was skipped"; exit 1; }' },
  ...commonSetupSteps.filter((step: any) => step.id !== 'cargo-cache'
    && step !== buildSnapshotRestore && step !== buildSnapshotPrepare)
    // The producer already restores the protected-main linux-tests Nix cache.
    // Reuse that tool/fixture cache here instead of cold, per-consumer entries.
    // Nix still resolves the pinned recipes; compiled test archives remain bound
    // separately to this exact successful producer, source, attempt and hashes.
    .map((step: any) => {
      if (step.id !== 'nix-cache') return step
      const key = step.with.key.replace('${{ github.job }}', 'linux-tests')
      if (!key.includes('nix5-linux-tests-')) throw new Error('shared fixture cache key must name linux-tests')
      return { ...step, with: { ...step.with, key,
        'restore-keys': step.with['restore-keys'].replaceAll('${{ github.job }}', 'linux-tests') } }
    }),
  ...testBuildSteps.slice(0, 2),
  {
    name: 'Download this run attempt’s successful test build',
    env: {
      GH_TOKEN: '${{ github.token }}',
      CI_TEST_ARCHIVE_ARTIFACT_ID: '${{ needs.linux-test-build.outputs.artifact-id }}',
      CI_TEST_ARCHIVE_MANIFEST_SHA256: '${{ needs.linux-test-build.outputs.manifest-sha256 }}',
      CI_TEST_ARCHIVE_PRODUCER_ATTEMPT: '${{ needs.linux-test-build.outputs.producer-attempt }}',
    },
    run: 'python3 scripts/ci-test-archive download',
  },
  { ...nixDevelopStep({ name: 'Verify source, hashes and extract test archives',
    command: ['python3', 'scripts/ci-test-archive', 'consume'] }),
    env: { CI_TEST_ARCHIVE_MANIFEST_SHA256: '${{ needs.linux-test-build.outputs.manifest-sha256 }}',
      CI_TEST_ARCHIVE_PRODUCER_ATTEMPT: '${{ needs.linux-test-build.outputs.producer-attempt }}' } },
  testBuildSteps[3],
]

/** Everything a job that runs the workspace tests needs. */
export const workspacePreparationSteps = [...commonSetupSteps, ...testBuildSteps]

/**
 * The stage jobs of `linux-gate` sign their own result. Every step is fail-soft and runs after the
 * stage's logs are retained, so signing can never change a stage's outcome. The signer scripts come
 * from the BASE commit of the change, never the tree under test. The note is written as a pack and
 * uploaded; the aggregate note job (shallow checkouts cannot push) publishes it.
 */
export const evidenceStageNames = [
  'linux-tests', 'linux-tests-shard-2', 'linux-tests-shard-3', 'linux-tests-shard-4',
  'linux-clippy', 'linux-fleet-compat', 'mail-redelivery-canaries',
] as const
const evidenceHelper = '"$RUNNER_TEMP/evidence-base/scripts/ci-evidence-signing"'
/**
 * Only the merge queue, main and pull requests from this repository reach the `evidence-signing`
 * environment (and so the key); a fork's pull request and any other event get an empty expression and run
 * without an environment, exactly as before. A same-repository author controls the workflow file its own
 * pull request runs, so its code can read the key (known defect 20). The
 * environment's own deployment-branch policy comes after this conditional exists.
 */
export const evidenceEnvironment =
  "${{ (github.event_name == 'merge_group' || (github.event_name == 'push' && github.ref == 'refs/heads/main') || (github.event_name == 'workflow_dispatch' && github.ref == 'refs/heads/main') || (github.event_name == 'pull_request' && github.event.pull_request.head.repo.full_name == github.repository)) && 'evidence-signing' || '' }}"
export const evidenceStageJob = (name: string) =>
  (evidenceStageNames as readonly string[]).includes(name) ? { environment: evidenceEnvironment } : {}

/**
 * Jobs that are not `scripts/ci-linux` stages but are required checks too (genie-freshness,
 * typescript-client, isolation-vm). They sign from the outcomes of their own steps; a step that did
 * not run (for example because the job was covered by a trusted note) makes no note.
 */
export const evidenceStepNames = ['genie-freshness', 'typescript-client', 'isolation-vm'] as const
export const evidenceStepJob = () => ({ environment: evidenceEnvironment })

// The evidence gate(s) that can cover a job's work. A job that took such a skip must not sign a note for a run in
// which it did not run its work: the signing helper refuses when this is 'true' (and so makes no note), whatever the
// job's log or step outcomes say. Every gate named here must be in that job's `needs`.
export const gateSkipExpression = (name: string): string => {
  const gates = name === 'mail-redelivery-canaries'
    ? ['linux-gate', 'mail-redelivery-canaries']
    : (evidenceStepNames as readonly string[]).includes(name) ? [name] : ['linux-gate']
  return '${{ ' + gates.map((gate) => `needs.evidence-gate-${gate}.outputs.skip == 'true'`).join(' || ') + ' }}'
}

const signingCore = (
  name: string,
  facts: { helper: 'stage-facts' | 'step-facts'; env: Record<string, string>; toolchain: boolean },
): unknown[] => {
  const soft = { if: 'always()', 'continue-on-error': true } as const
  // Without a toolchain override the wrapper asks nix for the dev shell derivation; jobs without a
  // stage shell fingerprint what they ran on instead.
  const toolchain = facts.toolchain
    ? ` \\
  --toolchain-sha256 "$(printf '%s|%s|%s|%s|%s|%s' "$RUNNER_OS" "$RUNNER_ARCH" "\${ImageVersion:-}" "$(nix --version 2>/dev/null)" "$(node --version 2>/dev/null)" "$(pnpm --version 2>/dev/null)" | sha256sum | cut -d' ' -f1)"`
    : ''
  return [
    {
      ...soft,
      name: 'Take the evidence signer from the base commit',
      id: 'evidence-base',
      env: { BASE_SHA: '${{ github.event.merge_group.base_sha || github.event.pull_request.base.sha || github.sha }}' },
      run: `rm -rf "$RUNNER_TEMP/evidence-base" && mkdir "$RUNNER_TEMP/evidence-base"
git cat-file -e "$BASE_SHA^{commit}" 2>/dev/null || git fetch --quiet --depth=1 origin "$BASE_SHA"
git archive "$BASE_SHA" scripts | tar -x -C "$RUNNER_TEMP/evidence-base"`,
    },
    {
      ...soft,
      name: 'May this job use the signing key?',
      id: 'evidence-guard',
      if: "${{ always() && steps.evidence-base.outcome == 'success' }}",
      env: {
        PR_HEAD_REPOSITORY: '${{ github.event.pull_request.head.repo.full_name }}',
        PR_BASE_SHA: '${{ github.event.pull_request.base.sha }}',
        PR_HEAD_SHA: '${{ github.event.pull_request.head.sha }}',
        PR_USER: '${{ github.event.pull_request.user.login }}',
      },
      run: `python3 ${evidenceHelper} guard`,
    },
    {
      ...soft,
      name: 'Write the signer configuration and key',
      id: 'evidence-configure',
      if: "${{ always() && steps.evidence-guard.outcome == 'success' }}",
      env: { EVIDENCE_SIGNING_KEY: '${{ secrets.EVIDENCE_SIGNING_KEY }}' },
      run: `set -o pipefail
python3 ${evidenceHelper} configure | tee "$RUNNER_TEMP/evidence-signer-home"`,
    },
    {
      ...soft,
      name: `Sign this ${facts.helper === 'stage-facts' ? 'stage' : 'job'} result`,
      if: "${{ always() && steps.evidence-configure.outcome == 'success' }}",
      env: { ...facts.env, GATE_SKIP: gateSkipExpression(name) },
      run: `helper=${evidenceHelper}
python3 "$helper" once ${facts.helper === 'stage-facts' ? '"$STAGE_JOB"' : '"$STEP_CHECK"'}
python3 "$helper" ${facts.helper} "$RUNNER_TEMP/job-facts.json"
home=$(tail -n 1 "$RUNNER_TEMP/evidence-signer-home")
HOME="$home" python3 "$RUNNER_TEMP/evidence-base/scripts/evidence-wrap" --check ${facts.helper === 'stage-facts' ? '"$STAGE_JOB"' : '"$STEP_CHECK"'} \\
  --aggregate "$RUNNER_TEMP/job-facts.json" --attempt "$GITHUB_RUN_ATTEMPT" \\
  --change-id "github:$GITHUB_REPOSITORY@$GITHUB_SHA" \\
  --signer '["python3","'"$RUNNER_TEMP"'/evidence-base/scripts/evidence-sign"]' \\
  --out "$RUNNER_TEMP/evidence-notes"${toolchain}`,
    },
    { ...soft, name: 'Remove the key', run: `python3 ${evidenceHelper} cleanup` },
    {
      ...soft,
      name: 'Retain the signed note',
      uses: 'actions/upload-artifact@v4',
      with: { name: `evidence-note-${name}`, path: '${{ runner.temp }}/evidence-notes/', 'if-no-files-found': 'ignore', 'retention-days': 7 },
    },
  ]
}

export const evidenceStageSteps = (name: string, stage: string): unknown[] =>
  (evidenceStageNames as readonly string[]).includes(name)
    ? signingCore(name, { helper: 'stage-facts', env: { STAGE: stage, STAGE_JOB: name }, toolchain: false })
    : []

/** Signing steps for a job made of steps: `command` names what they stand for, `steps` maps step ids to outcomes. */
export const evidenceStepSteps = (name: string, command: string[], stepIds: string[]): unknown[] =>
  signingCore(name, {
    helper: 'step-facts',
    env: {
      STEP_CHECK: name,
      STEP_COMMAND: JSON.stringify(command),
      STEP_OUTCOMES: '{' + stepIds.map((id) => `"${id}":"\${{ steps.${id}.outcome }}"`).join(',') + '}',
    },
    toolchain: true,
  })

// One Linux stage: its own runner and caches, the common setup, then scripts/ci-linux (or the
// command given), with steps before and after it.
export const linuxStageJob = ({
  name,
  stage,
  setup,
  cacheSaveSetup = setup,
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
  cacheSaveSetup?: readonly unknown[]
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
  ...evidenceStageJob(name),
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
    ...saveMainDependencyCaches(cacheSaveSetup),
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
    ...evidenceStageSteps(name, stage),
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

/** pnpm's store under the runner's temporary directory, which only step-level env can name. */
export const pnpmStoreEnv = { pnpm_config_store_dir: '${{ runner.temp }}/pnpm-store' } as const

/** The web lane's pnpm store, keyed by its lock and toolchain; only main upkeep fills it. */
export const fractalWebStoreCache = {
  path: pnpmStoreEnv.pnpm_config_store_dir,
  key: "fractal-web-pnpm-${{ runner.os }}-pnpm12.7.0-node24.20.0-${{ hashFiles('pnpm-lock.yaml') }}",
} as const

const embeddedPython = (script: string) =>
  `python3 - <<'PY'\n${readFileSync(new URL(`../../scripts/${script}`, import.meta.url), 'utf8')}\nPY`

// SHA-256(story ID) modulo this count; increasing it does not depend on index order.
const kitShardCount = 2

/**
 * The path-sensitive fractal-web gate always runs on GitHub-hosted capacity. Detection
 * needs no checkout; the gate checks out only to aggregate kit receipts. Execution runs
 * after generated files are fresh. Workflow path filters would leave required checks pending.
 */
export const fractalWebJobs = {
  'fractal-web-changes': {
    name: 'fractal-web-changes',
    'runs-on': 'ubuntu-latest',
    'timeout-minutes': 1,
    permissions: { contents: 'read' },
    outputs: {
      relevant: '${{ steps.detect.outputs.relevant }}',
      'kit-inputs': '${{ steps.detect.outputs.kit-inputs }}',
    },
    steps: [
      {
        name: 'Detect web-relevant changes',
        id: 'detect',
        env: {
          EVENT: '${{ github.event_name }}',
          REMOTE: '${{ github.server_url }}/${{ github.repository }}.git',
          BASE_SHA: '${{ github.event.pull_request.base.sha || github.event.merge_group.base_sha || github.event.before }}',
          HEAD_SHA: '${{ github.event.pull_request.head.sha || github.event.merge_group.head_sha || github.sha }}',
        },
        run: embeddedPython('ci-fractal-web-changes'),
      },
    ],
  },
  'fractal-web-execution': {
    name: 'fractal-web-execution',
    strategy: {
      'fail-fast': false,
      matrix: `\${{ fromJSON(needs.fractal-web-changes.outputs.kit-inputs == 'true' && '${JSON.stringify({ shard: Array.from({ length: kitShardCount }, (_, index) => index) })}' || '{"shard":[0]}') }}`,
    },
    needs: ['fractal-web-changes', 'genie-freshness'],
    if: "${{ !cancelled() && needs.fractal-web-changes.result == 'success' && needs.fractal-web-changes.outputs.relevant == 'true' && needs.genie-freshness.result == 'success' }}",
    'runs-on': 'ubuntu-latest',
    'timeout-minutes': 25,
    permissions: { contents: 'read' },
    defaults: { run: { shell: 'bash' } },
    steps: [
      { uses: 'actions/checkout@v4', with: { 'persist-credentials': false } },
      ...plainFlakeSetupSteps({ nix: { binaryCaches: readOnlyBinaryCaches } }),
      { name: 'Restore the pnpm store', id: 'pnpm-store', uses: 'actions/cache/restore@v4', with: fractalWebStoreCache },
      {
        ...nixDevelopStep({ name: 'Run the fractal-web lanes and dependency license check', flake: '.#web', command: ['bash', 'scripts/ci-fractal-web'] }),
        env: {
          ...pnpmStoreEnv,
          KIT_INPUTS: '${{ needs.fractal-web-changes.outputs.kit-inputs }}',
          KIT_SHARD_INDEX: '${{ matrix.shard }}',
          KIT_SHARD_COUNT: String(kitShardCount),
          KIT_RECEIPT_DIRECTORY: '${{ runner.temp }}/kit-play-receipts',
          BASE_SHA: '${{ github.event.pull_request.base.sha || github.event.merge_group.base_sha || github.event.before }}',
          HEAD_SHA: '${{ github.event.pull_request.head.sha || github.event.merge_group.head_sha || github.sha }}',
        },
      },
      {
        name: 'Preserve kit shard receipt',
        if: 'always()',
        uses: 'actions/upload-artifact@v4',
        with: {
          name: 'kit-play-${{ github.run_id }}-${{ github.sha }}-shard-${{ matrix.shard }}-attempt-${{ github.run_attempt }}',
          path: '${{ runner.temp }}/kit-play-receipts/receipt.json',
          'if-no-files-found': 'ignore',
        },
      },
    ],
  },
  'fractal-web': {
    name: 'fractal-web',
    needs: ['fractal-web-changes', 'fractal-web-execution'],
    // A failed, cancelled or unexpectedly skipped execution must fail this check.
    if: 'always()',
    'runs-on': 'ubuntu-latest',
    'timeout-minutes': 1,
    permissions: { contents: 'read' },
    steps: [
      {
        name: 'Require detection and, for web changes, execution',
        env: {
          CHANGES_RESULT: '${{ needs.fractal-web-changes.result }}',
          RELEVANT: '${{ needs.fractal-web-changes.outputs.relevant }}',
          EXECUTION_RESULT: '${{ needs.fractal-web-execution.result }}',
        },
        run: embeddedPython('ci-fractal-web-gate'),
      },
      // A successful matrix result alone cannot prove all expected shards ran.
      {
        uses: 'actions/checkout@v4',
        if: "needs.fractal-web-changes.outputs.kit-inputs == 'true' && needs.fractal-web-changes.outputs.relevant == 'true'",
        with: { 'persist-credentials': false },
      },
      {
        uses: 'actions/download-artifact@v4',
        if: "needs.fractal-web-changes.outputs.kit-inputs == 'true' && needs.fractal-web-changes.outputs.relevant == 'true'",
        // Include older attempts only from this exact run/head, for rerun-failed-jobs.
        with: { pattern: 'kit-play-${{ github.run_id }}-${{ github.sha }}-shard-*-attempt-*', path: '${{ runner.temp }}/kit-play-receipts' },
      },
      {
        name: 'Require every kit shard and exact full-index coverage',
        if: "needs.fractal-web-changes.outputs.kit-inputs == 'true' && needs.fractal-web-changes.outputs.relevant == 'true'",
        run: `node packages/fractal-ui/scripts/kit-play-shards.mjs "$RUNNER_TEMP/kit-play-receipts" ${kitShardCount}`,
      },
    ],
  },
} as const
