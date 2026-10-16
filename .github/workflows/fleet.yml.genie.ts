import { withQueueWatch } from './queue-watch.ts'
import { readyPRJobs, readyPullRequestTypes } from './ready-pr.ts'
import { buildSnapshotPrepare, buildSnapshotRestore, buildSnapshotSave, optionalQueueCacheSave } from './build-snapshot.ts'
import { auditCaches } from './cache-audit.ts'
import { readFileSync } from 'node:fs'
import {
  defaultActionlintConfig,
  githubWorkflow,
  nixDevelopStep,
} from '../../repos/effect-utils/genie/external.ts'
import {
  afterPickRunner,
  buildEnv,
  commonSetupSteps,
  fractalWebJobs,
  nixCacheStep,
  linuxRunner,
  linuxStageJob as namespaceStageJob,
  evidenceStageJob,
  evidenceEnvironment,
  evidenceStageNames,
  evidenceStageSteps,
  evidenceStepJob,
  evidenceStepNames,
  evidenceStepSteps,
  linuxStageRunner,
  linuxStageRunsOn,
  perfStoresCache,
  mailStageRunsOn,
  pickRunnerJob,
  pickRunnerJobId,
  primaryTestRunsOn,
  testBuildRunsOn,
  pnpmStoreEnv,
  supportingLinuxRunsOn,
  supportingStageRunsOn,
  secondaryStageRunsOn,
  extraMergeTestRunsOn,
  workspacePreparationSteps,
  testArchiveConsumerSetup,
} from './workspace-ci.ts'

// Namespace offers nested virtualization on linux/amd64. Prove /dev/kvm can create a VM before
// anything else; QEMU is also forbidden to fall back to emulation (nix/transport-isolation-vm.nix).
const kvmProbe = `if [ ! -e /dev/kvm ]; then
  echo "::error::/dev/kvm is missing: this runner profile offers no nested virtualization"
  exit 1
fi
if [ ! -r /dev/kvm ] || [ ! -w /dev/kvm ]; then sudo chmod 0666 /dev/kvm; fi
python3 - <<'EOF'
import fcntl, os
fd = os.open("/dev/kvm", os.O_RDWR | os.O_CLOEXEC)
version = fcntl.ioctl(fd, 0xAE00)  # KVM_GET_API_VERSION
vm = fcntl.ioctl(fd, 0xAE01, 0)  # KVM_CREATE_VM
assert version == 12, f"unexpected KVM API version {version}"
os.close(vm)
print(f"KVM API {version}: created a VM")
EOF
printf 'KVM: \\x60%s\\x60, CPU virtualization flag %s, VM creation succeeded\\n\\n| Phase | Elapsed |\\n| --- | --- |\\n' "$(ls -l /dev/kvm)" "$(grep -m1 -oE 'vmx|svm' /proc/cpuinfo || echo none)" >> "$GITHUB_STEP_SUMMARY"`

// One Linux gate stage: its own runner (ci1 or Namespace, see pickRunnerJob) and caches, the common
// setup, then scripts/ci-linux.
const linuxStageJob = ({
  name,
  stage,
  setup,
  description,
  env = {},
  before = [],
  extraLogs = '',
  runsOn = supportingStageRunsOn,
}: {
  name: string
  stage: string
  setup: readonly unknown[]
  description?: string
  env?: Record<string, string>
  before?: readonly unknown[]
  extraLogs?: string
  runsOn?: unknown
}) => ({
  name,
  ...afterPickRunner,
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
    nixDevelopStep({ name: description ?? 'Run nextest', command: ['bash', 'scripts/ci-linux', stage] }),
    {
      name: 'Save Nix outputs to the local Nix cache',
      if: `success() && env.CI_LOCAL_CACHES != '1' && ${optionalQueueCacheSave}`,
      run: 'bash scripts/ci-nix-cache save || echo "::warning::could not save the local Nix cache"',
    },
    ...(setup === testArchiveConsumerSetup ? [] : buildSnapshotSave),
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

// Required gate. Label events belong to macos.yml so they never restart or cancel this workflow.
// The Linux group. The jobs that are NOT required checks (the build, the test shards, clippy, fleet
// compatibility) are skipped at the JOB level, so no runner is allocated. A skipped member shows up as
// `skipped`, which is exactly what the aggregate evidence note producers refuse to sign for, so a skip cannot
// refresh its own note. `linux-gate` accepts `skipped` members only when its own gate said skip. The jobs that
// ARE required checks (mail-redelivery-canaries, isolation-vm) keep running and end in success with their steps
// skipped, so a required check never reports `skipped`.
// An existing step condition may be written as a whole `${{ ... }}`. Combining it with another term as text would
// leave extra characters around the markers, which GitHub reads as a literal and always-true string (actionlint
// reports it). Strip the markers first, then combine.
const bareCondition = (condition: unknown) => {
  const bare = String(condition).trim().replace(/^\$\{\{\s*|\s*\}\}$/g, '')
  // Two separate markers joined by an operator would leave markers inside; refuse to guess how to combine them.
  if (bare.includes('${{') || bare.includes('}}')) throw new Error(`step condition has markers inside it: ${String(condition)}`)
  return bare
}
const gateSkip = (gate: string) => `needs.evidence-gate-${gate}.outputs.skip == 'true'`
const gateRecord = (gate: string) => `\${{ needs.evidence-gate-${gate}.outputs.record }}`
const jobSkips: Record<string, { gates: string[]; skip: string }> = Object.fromEntries(
  ['linux-tests', 'linux-tests-shard-2', 'linux-tests-shard-3', 'linux-tests-shard-4', 'linux-clippy', 'linux-fleet-compat'].map((id) => [
    id,
    { gates: ['linux-gate'], skip: gateSkip('linux-gate') },
  ]),
)
// The aggregate note for linux-gate proves its member canaries passed, so either note covers the canaries.
const stepSkips: Record<string, { gates: string[]; skip: string }> = {
  'mail-redelivery-canaries': { gates: ['linux-gate', 'mail-redelivery-canaries'], skip: `${gateSkip('linux-gate')} || ${gateSkip('mail-redelivery-canaries')}` },
  'isolation-vm': { gates: ['isolation-vm'], skip: gateSkip('isolation-vm') },
}
// The aggregate's RESULTS are positional. Exactly the stages skipped at job level may report `skipped` under an
// evidence skip (derived here from the same list that skips them); every other stage, `upgrade-impact` and the
// canaries among them, must succeed. Positions after the base stages are the merge-only shards.
const aggregateStages = ['linux-test-build', 'upgrade-impact', 'linux-tests', 'linux-tests-shard-2', 'linux-clippy', 'linux-fleet-compat', 'mail-redelivery-canaries']
const mergeShards = ['linux-tests-shard-3', 'linux-tests-shard-4']
const skippablePositions = [...aggregateStages, ...mergeShards].flatMap((id, index) => (id === 'linux-test-build' || jobSkips[id] ? [index + 1] : []))
// Each skippable position carries the stage's OWN skip rule, evaluated by the workflow: a stage may report `skipped`
// under an evidence skip only when its own rule says skip, so a stage skipped for any other reason (a job-level
// predicate, an upstream problem) fails the gate even when the aggregate's gate also decided to skip. Positions
// that can never be covered carry a literal false.
const stageSkipFlags = (buildSkip: string) =>
  [...aggregateStages, ...mergeShards]
    .map((id) => (id === 'linux-test-build' ? `\${{ ${buildSkip} }}` : jobSkips[id] ? `\${{ ${jobSkips[id].skip} }}` : 'false'))
    .join(' ')
const skipStepsWhen = (steps: readonly any[], rule: { gates: string[]; skip: string }) => [
  ...steps.map((step) =>
    String(step.uses ?? '').startsWith('actions/checkout') ? step : { ...step, if: step.if == null ? `!(${rule.skip})` : `(${bareCondition(step.if)}) && !(${rule.skip})` },
  ),
  {
    name: 'evidence covered by a trusted note',
    if: rule.skip,
    env: { RECORDS: rule.gates.map(gateRecord).join(' ') },
    run: 'echo "covered by trusted evidence note: $RECORDS" >> "$GITHUB_STEP_SUMMARY"',
  },
]
const wireEvidenceSkips = (jobs: Record<string, any>) => {
  // The build feeds its dependents, so it may be skipped only when EVERY dependent is skipped, whether at job level
  // or step level: derived from their rules, never written twice, so the two cannot drift apart.
  const dependents = Object.keys(jobs).filter((id) => (jobSkips[id] || stepSkips[id]) && (jobs[id].needs ?? []).includes('linux-test-build'))
  const rules = dependents.map((id) => jobSkips[id] ?? stepSkips[id])
  const build = {
    gates: [...new Set(rules.flatMap((rule) => rule.gates))],
    skip: [...new Set(rules.map((rule) => `(${rule.skip})`))].join(' && '),
  }
  const withGates = (job: any, gates: string[]) => ({ ...job, needs: [...job.needs, ...gates.map((gate) => `evidence-gate-${gate}`)] })
  // The aggregate reads the build's rule too, so it needs every gate that rule reads.
  const withStageSkips = (job: any, rule: { gates: string[]; skip: string }) => ({
    ...withGates(job, rule.gates.filter((gate) => !job.needs.includes(`evidence-gate-${gate}`))),
    steps: job.steps.map((step: any) => (step.env?.GATE_SKIP ? { ...step, env: { ...step.env, STAGE_SKIPS: stageSkipFlags(rule.skip) } } : step)),
  })
  const jobLevel = (job: any, rule: { gates: string[]; skip: string }) => {
    const predicate = String(job.if).trim().replace(/^\$\{\{\s*|\s*\}\}$/g, '')
    return { ...withGates(job, rule.gates), if: `\${{ (${predicate}) && !(${rule.skip}) }}` }
  }
  return Object.fromEntries(
    Object.entries(jobs).map(([id, job]) => {
      if (id === 'linux-gate') return [id, withStageSkips(job, build)]
      if (id === 'linux-test-build') return [id, jobLevel(job, build)]
      if (jobSkips[id]) return [id, jobLevel(job, jobSkips[id])]
      if (stepSkips[id]) return [id, { ...withGates(job, stepSkips[id].gates), steps: skipStepsWhen(job.steps, stepSkips[id]) }]
      return [id, job]
    }),
  )
}

// Skip a check's heavy steps when its evidence gate decided to skip (docs/st3/gate-evidence.md). The job
// itself still runs and ends in success, so the check keeps its name and every `needs` on it keeps working.
// The checkout is never skipped (the job's own cache-coverage report reads the repository), and a gate job
// that failed, was skipped or never started has no output, so the steps run exactly as before.
const skipIfCovered = (check: string, steps: readonly any[]) => {
  const covered = `needs.evidence-gate-${check}.outputs.skip != 'true'`
  return [
    ...steps.map((step) =>
      String(step.uses ?? '').startsWith('actions/checkout')
        ? step
        // An existing condition may be written as a whole `${{ ... }}`; combining it as text would leave extra
        // characters around the markers, which GitHub reads as a literal and always-true string. Strip them first.
        : { ...step, if: step.if == null ? covered : `(${bareCondition(step.if)}) && ${covered}` },
    ),
    {
      name: 'evidence covered by a trusted note',
      if: `needs.evidence-gate-${check}.outputs.skip == 'true'`,
      env: { RECORD: `\${{ needs.evidence-gate-${check}.outputs.record }}` },
      run: 'echo "covered by trusted evidence note $RECORD" >> "$GITHUB_STEP_SUMMARY"',
    },
  ]
}

// Shadow mode: for each required check, ask the verifier from the accepted base whether a trusted note
// exists for this run's tree and record the outcome in a fixed step name a collector can count.
// It never skips a check, never fails, and is not itself a required check.
const evidenceGateChecks = ['linux-gate', 'typescript-client', 'isolation-vm', 'genie-freshness', 'mail-redelivery-canaries']
const evidenceGateOutcomes = [
  ['satisfied', 'evidence satisfied by existing note'],
  ['degraded', 'evidence existing note is degraded'],
  ['no_note', 'evidence no note'],
  ['rejected', 'evidence note rejected'],
  ['unavailable', 'evidence verifier unavailable'],
] as const
const evidenceGateJobs = Object.fromEntries(evidenceGateChecks.map((check) => [`evidence-gate-${check}`, {
  name: `evidence-gate-${check}`,
  'runs-on': 'ubuntu-latest',
  'timeout-minutes': 5,
  'continue-on-error': true,
  permissions: { contents: 'read' },
  // The decision the check jobs act on comes from the marker step below, not from the gate step: skip=true can
  // exist only if the marker already ran, so the observer, which keys on the marker, can never miss a skip.
  outputs: { skip: '${{ steps.taken.outputs.skip }}', record: '${{ steps.taken.outputs.record }}' },
  steps: [
    { uses: 'actions/checkout@v4', with: { 'persist-credentials': false } },
    {
      name: 'Look for an existing evidence note',
      id: 'gate',
      env: {
        CHECK: check,
        REPOSITORY: '${{ github.repository }}',
        BASE_SHA: "${{ github.event.pull_request.base.sha || github.event.merge_group.base_sha || (github.event_name == 'push' && github.event.before) || '' }}",
        HEAD_SHA: '${{ github.sha }}',
        HEAD_REPOSITORY: "${{ github.event.pull_request.head.repo.full_name || ((github.event_name == 'merge_group' || github.event_name == 'push') && github.repository) || '' }}",
        EVENT_NAME: '${{ github.event_name }}',
        PR_NUMBER: '${{ github.event.pull_request.number }}',
        // A pull request run may ask about the pull request HEAD's tree (scripts/ci-evidence-gate, choose_tree); these are
        // the payload facts it checks that head and its authors against. All empty outside a pull request.
        PR_HEAD_SHA: '${{ github.event.pull_request.head.sha }}',
        PR_USER: '${{ github.event.pull_request.user.login }}',
        PR_DRAFT: '${{ github.event.pull_request.draft }}',
        ACTOR: '${{ github.actor }}',
        TRIGGERING_ACTOR: '${{ github.triggering_actor }}',
        RUN_ID: '${{ github.run_id }}',
        RUN_ATTEMPT: '${{ github.run_attempt }}',
      },
      // The gate is taken from the accepted base, like the verifier, so a change cannot rewrite the
      // script that decides whether it is skipped. No gate at the base, or any failure: run the check.
      run: `git fetch --no-tags --depth=1 -q origin "$BASE_SHA" && git show "$BASE_SHA:scripts/ci-evidence-gate" > "$RUNNER_TEMP/ci-evidence-gate" && python3 "$RUNNER_TEMP/ci-evidence-gate" || { printf 'outcome=unavailable\\nskip=false\\nrecord=-\\n' >> "$GITHUB_OUTPUT"; echo "evidence-decision v1 check=$CHECK decision=run outcome=unavailable reason=no-gate-at-base run=$RUN_ID"; }`,
    },
    ...evidenceGateOutcomes.map(([outcome, name]) => ({
      name,
      if: `steps.gate.outputs.outcome == '${outcome}'`,
      run: `echo ${name}`,
    })),
    // Fixed name: a decision to skip was taken. The observer must not record a check whose gate ran
    // this step as a fresh finished run, or one real run would keep refreshing its own skips.
    {
      name: 'evidence skip taken',
      id: 'taken',
      if: "steps.gate.outputs.skip == 'true'",
      env: { RECORD: '${{ steps.gate.outputs.record }}' },
      // The outputs are the LAST thing this step writes: if anything before them fails, there is no skip.
      run: `echo evidence skip taken && printf 'skip=true\\nrecord=%s\\n' "$RECORD" >> "$GITHUB_OUTPUT"`,
    },
  ],
}]))

export default githubWorkflow(withQueueWatch(readyPRJobs(auditCaches({
  name: 'Workspace CI',
  on: {
    pull_request: { types: readyPullRequestTypes },
    // GitHub's merge queue runs the required checks on each queued entry; without this trigger the
    // queue never receives them and merges freeze.
    merge_group: {},
    // The queue's successful checks belong to the exact commit that lands on main. Main upkeep
    // preserves perf-cost and fills missing default-branch caches without repeating this gate.
    workflow_dispatch: {},
  },
  permissions: { contents: 'read', actions: 'read' },
  concurrency: {
    // PR updates replace stale checks; every other run has its own group so pending pushes survive.
    group: 'workspace-${{ github.event.pull_request.number || github.run_id }}-${{ github.event_name }}',
    'cancel-in-progress': '${{ github.event_name == \'pull_request\' }}',
  },
  // actionlint must know the Namespace shape label the stage jobs use.
  actionlint: {
    ...defaultActionlintConfig,
    selfHostedRunnerLabels: [...(defaultActionlintConfig.selfHostedRunnerLabels ?? []), ...linuxRunner, ...linuxStageRunner],
  },
  jobs: wireEvidenceSkips({
    [pickRunnerJobId]: pickRunnerJob,
    // Non-required provenance only: it runs alongside the gate and has no signer or key.
    // Retain the complete effective merge tree even after its ephemeral ref disappears.
    'evidence-source': {
      name: 'evidence-source',
      'runs-on': 'ubuntu-latest',
      'timeout-minutes': 5,
      'continue-on-error': true,
      permissions: { contents: 'read' },
      steps: [
        { uses: 'actions/checkout@v4', with: { 'persist-credentials': false } },
        {
          name: 'Retain effective tested source',
          // Execute the workflow's embedded producer outside the checkout. It observes Git
          // objects only; it never executes source or carries a production signing key.
          run: `cat > "$RUNNER_TEMP/ci-evidence-source-producer.py" <<'EVIDENCE_SOURCE_PY'\n${readFileSync(new URL('../../scripts/ci-evidence-source', import.meta.url), 'utf8').trimEnd()}\nEVIDENCE_SOURCE_PY\npython3 "$RUNNER_TEMP/ci-evidence-source-producer.py"`,
        },
        {
          name: 'Upload effective tested source',
          uses: 'actions/upload-artifact@v4',
          with: {
            name: 'ci-evidence-source-${{ github.run_attempt }}',
            path: '${{ runner.temp }}/ci-evidence-source/',
            'if-no-files-found': 'error',
            'retention-days': 14,
          },
        },
      ],
    },
    ...evidenceGateJobs,
    // Start immediately on hosted capacity: no Nix setup, downloads or compilation.
    'upgrade-impact': {
      name: 'upgrade-impact',
      'runs-on': 'ubuntu-latest',
      'timeout-minutes': 5,
      steps: [
        { uses: 'actions/checkout@v4', with: { 'fetch-depth': 0, 'persist-credentials': false } },
        { name: 'Test release and PR classification safety', run: 'python3 scripts/release_notes_test.py' },
        {
          name: 'Require a fresh fragment on the effective PR or queue merge',
          env: {
            IMPACT_BASE_SHA: '${{ github.event.merge_group.base_sha }}',
            IMPACT_PR_NUMBER: '${{ github.event.pull_request.number }}',
            IMPACT_QUEUE_REF: '${{ github.event.merge_group.head_ref }}',
          },
          run: `if [ -n "$IMPACT_PR_NUMBER" ]; then
  impact_pr_base=$(git rev-parse "$GITHUB_SHA^1")
  python3 scripts/check-release-impact --base "$impact_pr_base" --source "$GITHUB_SHA" --pr-number "$IMPACT_PR_NUMBER"
elif [ -n "$IMPACT_BASE_SHA" ]; then
  python3 scripts/check-release-impact --base "$IMPACT_BASE_SHA" --source "$GITHUB_SHA" --queue-ref "$IMPACT_QUEUE_REF"
else
  echo "Manual dispatch has no PR/queue delta; classification safety tests passed."
fi`,
        },
      ],
    },
    // Watch queue refs on GitHub-hosted capacity, even when both workload pools are occupied.
    'namespace-capacity': {
      name: 'namespace-capacity',
      if: "github.event_name == 'merge_group'",
      'runs-on': 'ubuntu-latest',
      'timeout-minutes': 120,
      permissions: { contents: 'read', actions: 'write' },
      defaults: { run: { shell: 'bash' } },
      steps: [
        {
          name: 'Cancel superseded merge groups',
          if: "github.event_name == 'merge_group'",
          env: {
            GH_TOKEN: '${{ github.token }}',
            REPOSITORY: '${{ github.repository }}',
            RUN_ID: '${{ github.run_id }}',
            QUEUE_REF: '${{ github.event.merge_group.head_ref }}',
            QUEUE_SHA: '${{ github.event.merge_group.head_sha }}',
          },
          // Embed trusted workflow source: this job never checks out or executes queued PR code.
          run: `python3 - <<'QUEUE_WATCH_PY'\n${readFileSync(new URL('../../scripts/ci-queue-watch', import.meta.url), 'utf8')}\nQUEUE_WATCH_PY`,
        },
      ],
    },
    'genie-freshness': {
      name: 'genie-freshness',
      env: { CI_CACHE_DEV_SHELL: 'genie' },
      ...afterPickRunner,
      needs: [...afterPickRunner.needs, 'evidence-gate-genie-freshness'],
      'runs-on': 'ubuntu-latest',
      'timeout-minutes': 20,
      ...evidenceStepJob(),
      steps: skipIfCovered('genie-freshness', [
        ...commonSetupSteps.filter((step) => !('id' in step && step.id === 'cargo-cache') && step !== buildSnapshotPrepare && step !== buildSnapshotRestore),
        { ...nixDevelopStep({ name: 'Check license coverage, runner selection and generated files', flake: '.#genie', command: ['bash', 'scripts/ci-genie-freshness'] }), id: 'freshness' },
        { name: 'Save Nix outputs', if: `success() && env.CI_LOCAL_CACHES != '1' && ${optionalQueueCacheSave}`, run: 'bash scripts/ci-nix-cache save' },
        ...evidenceStepSteps('genie-freshness', ['bash', 'scripts/ci-genie-freshness'], ['freshness']),
      ]),
    },
    // Check the shared client and its iOS consumer before merge.
    'typescript-client': {
      name: 'typescript-client',
      // Keep generated-file validation ahead of its consumers.
      needs: ['pick-runner', 'genie-freshness', 'evidence-gate-typescript-client'],
      if: "${{ !cancelled() && needs.genie-freshness.result == 'success' }}",
      'runs-on': 'ubuntu-latest',
      'timeout-minutes': 10,
      ...evidenceStepJob(),
      defaults: { run: { shell: 'bash' } },
      env: { COREPACK_ENABLE_DOWNLOAD_PROMPT: '0' },
      steps: skipIfCovered('typescript-client', [
        { uses: 'actions/checkout@v4', with: { 'persist-credentials': false } },
        // Node 24, as in the workspace shell; schema tests use its native TypeScript loading.
        { uses: 'actions/setup-node@v4', with: { 'node-version': '24.18.0' } },
        {
          name: 'Fingerprint the locked dependencies',
          id: 'lockfiles',
          // ci1's Nix runner lacks the Node 20 helper used by GitHub's hashFiles expression.
          run: `lockfiles_hash=$(sha256sum pnpm-lock.yaml | cut -d ' ' -f1)
printf 'hash=%s\\n' "$lockfiles_hash" >> "$GITHUB_OUTPUT"`,
        },
        {
          name: 'Cache the locked TypeScript and Effect toolchain',
          id: 'typescript-cache',
          uses: 'actions/cache@v5',
          with: {
            path: pnpmStoreEnv.pnpm_config_store_dir,
            key: 'typescript-client-${{ runner.os }}-node24.18.0-pnpm12.7.0-${{ steps.lockfiles.outputs.hash }}',
          },
        },
        // Corepack provisions the root manifest's pnpm@12.7.0; the workspace forbids install scripts.
        { name: 'Install the frozen workspace', id: 'install', env: pnpmStoreEnv, run: 'corepack enable\npnpm install --frozen-lockfile' },
        { name: 'Check the client, shared views and iOS consumers', id: 'client', run: 'bash scripts/ci-typescript-client' },
        ...evidenceStepSteps('typescript-client', ['bash', 'scripts/ci-typescript-client'], ['install', 'client']),
      ]),
    },
    'linux-test-build': {
      name: 'linux-test-build',
      ...afterPickRunner,
      'runs-on': testBuildRunsOn,
      'timeout-minutes': 120,
      env: { ...buildEnv, CI_CACHE_DEV_SHELL: 'default' },
      outputs: {
        'artifact-id': '${{ steps.upload.outputs.artifact-id }}',
        'manifest-sha256': '${{ steps.archive.outputs.manifest-sha256 }}',
        'producer-attempt': '${{ steps.archive.outputs.producer-attempt }}',
      },
      steps: [
        ...workspacePreparationSteps.slice(0, -2).map((step: any) =>
          step.id === 'cargo-cache' || step.id === 'nix-cache'
            ? { ...step, with: { ...step.with, key: step.with.key.replace('${{ github.job }}', 'linux-tests'),
                'restore-keys': step.with['restore-keys'].replaceAll('${{ github.job }}', 'linux-tests') } }
            : step),
        { ...nixDevelopStep({ name: 'Compile and archive each selected target group once',
          command: ['bash', '-c', 'python3 scripts/ci-build-memory before || echo "::warning::build memory baseline unavailable"; exec python3 scripts/ci-test-archive build'] }), id: 'archive' },
        { name: 'Report test-build memory after success',
          env: { BUILD_STEP_OUTCOME: '${{ steps.archive.outcome }}' },
          run: 'python3 scripts/ci-build-memory after',
          if: "${{ success() && steps.archive.outcome == 'success' }}", 'continue-on-error': true },
        {
          name: 'Publish this run attempt’s exact-source test archives', id: 'upload',
          uses: 'actions/upload-artifact@v4',
          with: { name: 'linux-test-archives-${{ github.run_attempt }}',
            path: '${{ runner.temp }}/ci-test-archives', 'if-no-files-found': 'error',
            'compression-level': 0, 'retention-days': 3 },
        },
        ...buildSnapshotSave,
        { name: 'Report test-build memory after failure',
          env: { BUILD_STEP_OUTCOME: '${{ steps.archive.outcome }}' },
          run: 'mkdir -p "$RUNNER_TEMP/ci-build-memory"; (free -m || echo "::warning::free unavailable") | tee "$RUNNER_TEMP/ci-build-memory/free.txt"; python3 scripts/ci-build-memory failure',
          if: "${{ failure() && steps.archive.outcome == 'failure' }}", 'continue-on-error': true },
        { name: 'Retain test-build memory evidence',
          if: "${{ always() && (steps.archive.outcome == 'success' || steps.archive.outcome == 'failure') }}", 'continue-on-error': true,
          uses: 'actions/upload-artifact@v4', with: { name: 'test-build-memory-${{ github.run_attempt }}',
            path: '${{ runner.temp }}/ci-build-memory', 'if-no-files-found': 'warn', 'retention-days': 3 } },
      ],
    },
    // Four merge partitions (two for other events) reuse the one producer archive.
    // `linux-gate` below is the single required check that collects them.
    'linux-tests': {
      ...linuxStageJob({
        name: 'linux-tests',
        stage: 'tests',
        setup: testArchiveConsumerSetup,
        runsOn: primaryTestRunsOn,
        // CI_RUN_ID keeps the messaging-fault evidence under target/messaging-faults and a failed
        // boot canary's evidence under target/boot-canaries.
        env: { CI_RUN_ID: '${{ github.run_id }}', CI_TEST_PARTITION: "${{ github.event_name == 'merge_group' && 'hash:1/4' || 'hash:1/2' }}", CI_TEST_THREADS: '8' },
        extraLogs: 'target/messaging-faults/\ntarget/boot-canaries/',
        before: [nixDevelopStep({ name: 'Prove shards form the complete selected inventory', command: ['python3', 'scripts/ci-test-partitions'] })],
      }),
      needs: [pickRunnerJobId, 'linux-test-build'],
      if: "${{ !cancelled() }}",
    },
    'linux-tests-shard-2': {
      ...linuxStageJob({
        name: 'linux-tests-shard-2',
        stage: 'tests',
        runsOn: secondaryStageRunsOn,
        setup: testArchiveConsumerSetup,
        env: { CI_RUN_ID: '${{ github.run_id }}', CI_TEST_PARTITION: "${{ github.event_name == 'merge_group' && 'hash:2/4' || 'hash:2/2' }}", CI_TEST_THREADS: '8' },
        extraLogs: 'target/messaging-faults/\ntarget/boot-canaries/',
      }),
      needs: [pickRunnerJobId, 'linux-test-build'],
      if: "${{ !cancelled() }}",
    },
    'linux-tests-shard-3': {
      ...linuxStageJob({
        name: 'linux-tests-shard-3', stage: 'tests', setup: testArchiveConsumerSetup,
        runsOn: extraMergeTestRunsOn,
        env: { CI_RUN_ID: '${{ github.run_id }}', CI_TEST_PARTITION: 'hash:3/4', CI_TEST_THREADS: '8' },
        extraLogs: 'target/messaging-faults/\ntarget/boot-canaries/',
      }),
      needs: [pickRunnerJobId, 'linux-test-build'],
      if: "${{ !cancelled() && github.event_name == 'merge_group' }}",
    },
    'linux-tests-shard-4': {
      ...linuxStageJob({
        name: 'linux-tests-shard-4', stage: 'tests', setup: testArchiveConsumerSetup,
        runsOn: extraMergeTestRunsOn,
        env: { CI_RUN_ID: '${{ github.run_id }}', CI_TEST_PARTITION: 'hash:4/4', CI_TEST_THREADS: '8' },
        extraLogs: 'target/messaging-faults/\ntarget/boot-canaries/',
      }),
      needs: [pickRunnerJobId, 'linux-test-build'],
      if: "${{ !cancelled() && github.event_name == 'merge_group' }}",
    },
    'linux-clippy': linuxStageJob({
      name: 'linux-clippy',
      stage: 'clippy',
      setup: commonSetupSteps,
      description: 'Run the Clippy warning ratchet and generated-client check',
      // Compare to the event's immutable base, including queued merges and main pushes.
      env: { CLIPPY_BASE_SHA: '${{ github.event.pull_request.base.sha || github.event.merge_group.base_sha || github.event.before }}' },
    }),
    'linux-fleet-compat': linuxStageJob({
      name: 'linux-fleet-compat',
      stage: 'fleet-compat',
      setup: commonSetupSteps,
      description: 'Run fleet compatibility against the pinned older st3',
    }),
    'mail-redelivery-canaries': {
      ...namespaceStageJob({
        name: 'mail-redelivery-canaries',
        stage: 'mail-redelivery-canaries',
        runsOn: mailStageRunsOn,
        description: 'Require every harness to hold old mail across boot and reconnect',
        setup: testArchiveConsumerSetup,
        // Restore shared fixtures, but keep main-upkeep as their single cache writer.
        cacheSaveSetup: [nixCacheStep],
        env: { CI_RUN_ID: '${{ github.run_id }}', CI_TEST_THREADS: '8' },
        extraLogs: 'target/messaging-faults/\ntarget/boot-canaries/',
      }),
      needs: [pickRunnerJobId, 'linux-test-build'],
      if: "${{ !cancelled() }}",
    },
    'linux-gate': {
      name: 'linux-gate',
      needs: [pickRunnerJobId, 'evidence-gate-linux-gate', 'linux-test-build', 'upgrade-impact', 'linux-tests', 'linux-tests-shard-2', 'linux-tests-shard-3', 'linux-tests-shard-4', 'linux-clippy', 'linux-fleet-compat', 'mail-redelivery-canaries'],
      // A skipped or cancelled stage must fail the gate, so it runs even when a stage failed.
      if: 'always()',
      // Aggregation needs no build caches and must not queue behind the work it summarizes.
      'runs-on': 'ubuntu-latest',
      'timeout-minutes': 5,
      steps: [
        {
          name: 'Require every Linux stage to pass',
          // The stages only: pick-runner is skipped whenever ci1 is off.
          env: { GATE_SKIP: '${{ needs.evidence-gate-linux-gate.outputs.skip }}', RESULTS: aggregateStages.map((id) => `\${{ needs.${id}.result }}`).join(' '), EVENT: '${{ github.event_name }}', MERGE_RESULTS: '${{ needs.linux-tests-shard-3.result }} ${{ needs.linux-tests-shard-4.result }}' },
          run: `set -eu
set -- $RESULTS
[ "$#" = 7 ] || { echo "incomplete base-stage results"; exit 1; }
if [ "$EVENT" = merge_group ]; then
  set -- $MERGE_RESULTS
  [ "$#" = 2 ] || { echo "incomplete merge-shard results"; exit 1; }
  RESULTS="$RESULTS $MERGE_RESULTS"
else
  [ "$MERGE_RESULTS" = "skipped skipped" ] || { echo "extra shards must be merge-only"; exit 1; }
fi
echo "stage results: $RESULTS (evidence skip: \${GATE_SKIP:-none})"
position=0
for result in $RESULTS; do
  position=$((position + 1))
  [ "$result" = success ] && continue
  # A stage the evidence gate skipped reports \`skipped\`: accepted only when this gate decided to skip, only
  # in the positions of stages skipped at job level, only when that stage's own skip rule says skip, and never
  # for a failure or a cancellation.
  flag=$(printf '%s\\n' "\${STAGE_SKIPS:-}" | cut -d' ' -f"$position")
  if [ "\${GATE_SKIP:-}" = true ] && [ "$flag" = true ] && [ "$result" = skipped ]; then
    case "$position" in ${skippablePositions.join('|')}) continue ;; esac
  fi
  exit 1
done`,
        },
      ],
    },
    // Signs what the aggregate gate knows, with the key of the `evidence-signing` environment. It is a
    // separate job so the required `linux-gate` never depends on the environment, the key or the push:
    // every step is fail-soft and a signing outage changes nothing about the gate. Forks never get the
    // key. The signer scripts come from the BASE commit of the change, never from the tree under test.
    'evidence-note-linux-gate': {
      name: 'evidence-note-linux-gate',
      needs: ['linux-gate', 'linux-test-build', 'upgrade-impact', 'linux-tests', 'linux-tests-shard-2', 'linux-tests-shard-3', 'linux-tests-shard-4', 'linux-clippy', 'linux-fleet-compat', 'mail-redelivery-canaries', ...evidenceStepNames, 'evidence-gate-linux-gate', 'evidence-gate-mail-redelivery-canaries', 'evidence-gate-isolation-vm', 'evidence-gate-genie-freshness', 'evidence-gate-typescript-client'],
      // The merge queue, main and pull requests from this repository sign; a fork never gets the key.
      if: "${{ always() && (github.event_name == 'merge_group' || (github.event_name == 'push' && github.ref == 'refs/heads/main') || (github.event_name == 'workflow_dispatch' && github.ref == 'refs/heads/main') || (github.event_name == 'pull_request' && github.event.pull_request.head.repo.full_name == github.repository)) }}",
      'runs-on': 'ubuntu-latest',
      environment: evidenceEnvironment,
      permissions: { contents: 'write' },
      'timeout-minutes': 15,
      defaults: { run: { shell: 'bash' } },
      env: {
        EVIDENCE_PUSH_TOKEN: '${{ github.token }}',
        PR_HEAD_REPOSITORY: '${{ github.event.pull_request.head.repo.full_name }}',
        PR_BASE_SHA: '${{ github.event.pull_request.base.sha }}',
        PR_HEAD_SHA: '${{ github.event.pull_request.head.sha }}',
        PR_USER: '${{ github.event.pull_request.user.login }}',
        NEEDS_JSON: '${{ toJSON(needs) }}',
      },
      steps: [
        // The tested commit with its whole history: a note's parent is that commit, and the server
        // refuses a push from a shallow repository.
        { name: 'Check out the tested commit', 'continue-on-error': true, uses: 'actions/checkout@v4', with: { 'fetch-depth': 0, 'persist-credentials': false } },
        {
          name: 'Take the signer scripts from the base commit',
          id: 'base',
          'continue-on-error': true,
          env: { BASE_SHA: '${{ github.event.merge_group.base_sha || github.event.pull_request.base.sha || github.sha }}' },
          run: `rm -rf "$RUNNER_TEMP/evidence-base" && mkdir "$RUNNER_TEMP/evidence-base"
git cat-file -e "$BASE_SHA^{commit}" 2>/dev/null || git fetch --quiet origin "$BASE_SHA"
git archive "$BASE_SHA" scripts | tar -x -C "$RUNNER_TEMP/evidence-base"`,
        },
        { name: 'May this job use the signing key?', id: 'guard', 'continue-on-error': true, run: 'python3 "$RUNNER_TEMP/evidence-base/scripts/ci-evidence-signing" guard' },
        {
          name: 'Write the signer configuration and key',
          id: 'configure',
          if: "${{ steps.guard.outcome == 'success' }}",
          'continue-on-error': true,
          env: { EVIDENCE_SIGNING_KEY: '${{ secrets.EVIDENCE_SIGNING_KEY }}' },
          run: `set -o pipefail
python3 "$RUNNER_TEMP/evidence-base/scripts/ci-evidence-signing" configure | tee "$RUNNER_TEMP/evidence-signer-home"`,
        },
        {
          name: 'Sign the linux-gate note',
          id: 'sign',
          if: "${{ steps.configure.outcome == 'success' }}",
          'continue-on-error': true,
          run: `helper="$RUNNER_TEMP/evidence-base/scripts/ci-evidence-signing"
python3 "$helper" once linux-gate
python3 "$helper" aggregate "$RUNNER_TEMP/gate-facts.json"
home=$(tail -n 1 "$RUNNER_TEMP/evidence-signer-home")
HOME="$home" python3 "$RUNNER_TEMP/evidence-base/scripts/evidence-wrap" --check linux-gate \\
  --aggregate "$RUNNER_TEMP/gate-facts.json" --attempt "$GITHUB_RUN_ATTEMPT" \\
  --change-id "github:$GITHUB_REPOSITORY@$GITHUB_SHA" --toolchain-sha256 "$(printf 'github-hosted|%s' "\${ImageVersion:-unknown}" | sha256sum | cut -d' ' -f1)" \\
  --signer '["python3","'"$RUNNER_TEMP"'/evidence-base/scripts/evidence-sign"]' --out "$RUNNER_TEMP/evidence-notes/gate"`,
        },
        // The stage jobs' signed notes, written as packs by each stage's own signing steps.
        {
          name: 'Collect the stage notes',
          if: "${{ steps.sign.outcome == 'success' }}",
          'continue-on-error': true,
          uses: 'actions/download-artifact@v4',
          with: { pattern: 'evidence-note-*', path: '${{ runner.temp }}/evidence-notes/stages', 'merge-multiple': true },
        },
        {
          name: 'Publish the notes that verify',
          if: "${{ steps.sign.outcome == 'success' }}",
          'continue-on-error': true,
          env: { BASE_SHA: '${{ github.event.merge_group.base_sha || github.event.pull_request.base.sha || github.sha }}' },
          run: `git show "$BASE_SHA:.st/evidence-keys" > "$RUNNER_TEMP/evidence-keys" || { echo "::warning::no .st/evidence-keys at the base commit; nothing is published"; exit 1; }
python3 "$RUNNER_TEMP/evidence-base/scripts/evidence-publish" --notes "$RUNNER_TEMP/evidence-notes" \\
  --keys "$RUNNER_TEMP/evidence-keys" --checks "linux-gate,${[...evidenceStageNames, ...evidenceStepNames].join(',')}" --remote origin`,
        },
        { name: 'Remove the key', if: 'always()', 'continue-on-error': true, run: 'python3 "$RUNNER_TEMP/evidence-base/scripts/ci-evidence-signing" cleanup' },
      ],
    },
    // The cost check: SQLite work per daemon request on a small and a ten times larger generated
    // store. Counts, not timings, so a lightly optimized build only speeds up the generation.
    // Not part of linux-gate; it must finish before linux-tests does (docs/ci.md). The load test
    // runs in perf.yml. It runs where the stages run, and skips merge-queue entries, which do not
    // wait for it, so each queued entry still needs only its required jobs' capacity. A required
    // perf-cost must run there too.
    'perf-cost': namespaceStageJob({
      name: 'perf-cost',
      stage: 'cost',
      runsOn: linuxStageRunner,
      condition: "github.event_name != 'merge_group'",
      setup: commonSetupSteps,
      description: 'Run the cost check',
      command: ['bash', 'scripts/ci-perf', 'cost'],
      env: { CARGO_PROFILE_DEV_OPT_LEVEL: '1' },
      extraLogs: '${{ runner.temp }}/perf/',
      before: [perfStoresCache('cost')],
    }),
    // st2's transport-isolation cascade tests need a real systemd user manager, which the
    // runner image lacks. A NixOS VM runs this job's prebuilt test binary; it compiles nothing.
    'isolation-vm': {
      name: 'isolation-vm',
      needs: [pickRunnerJobId, 'linux-test-build'],
      if: "${{ !cancelled() }}",
      'runs-on': supportingLinuxRunsOn,
      'timeout-minutes': 60,
      ...evidenceStepJob(),
      defaults: { run: { shell: 'bash' } },
      env: { ...buildEnv, CI_CACHE_DEV_SHELL: 'default' },
      steps: [
        // The VM's own warm cache also contains its driver; keep that primary entry.
        ...testArchiveConsumerSetup.map((step: any) => step.id === 'nix-cache' ? nixCacheStep : step),
        { name: 'Probe KVM', id: 'kvm', run: kvmProbe },
        {
          name: 'Build the NixOS VM test driver',
          id: 'driver',
          run: `start=$SECONDS
nix build --print-build-logs --out-link "$RUNNER_TEMP/vm-driver" .#legacyPackages.x86_64-linux.transport-isolation-vm.driver
printf '| VM driver build | %ss |\\n' "$((SECONDS - start))" >> "$GITHUB_STEP_SUMMARY"`,
        },
        {
          name: 'Run all three systemd-scope tests in the VM',
          id: 'vmtest',
          env: {
            ST_ISOLATION_ARCHIVE: '${{ runner.temp }}/ci-test-archives/workspace.tar.zst',
            ST_ISOLATION_WORKSPACE: '${{ github.workspace }}',
            ST_ISOLATION_TIMINGS: '${{ runner.temp }}/vm-timings.json',
            ST_ISOLATION_BUILD_WORKSPACE: '${{ env.CI_TEST_BUILD_WORKSPACE }}',
          },
          run: `start=$SECONDS
mkdir -p "$RUNNER_TEMP/vm-out"
"$RUNNER_TEMP/vm-driver/bin/nixos-test-driver" --output_directory "$RUNNER_TEMP/vm-out"
jq -r '"| VM boot | \\(.boot_seconds)s |\\n| systemd-scope tests in VM (extract and run) | \\(.test_seconds)s |"' "$ST_ISOLATION_TIMINGS" >> "$GITHUB_STEP_SUMMARY"
printf '| VM test driver total | %ss |\\n' "$((SECONDS - start))" >> "$GITHUB_STEP_SUMMARY"`,
        },
        // The sekrets gateway needs real Unix users, a login session and a user manager: a second
        // VM runs it with this st binary (nix/sekrets-vm.nix).
        {
          name: 'Build the sekrets VM test driver',
          id: 'sekrets-driver',
          run: `start=$SECONDS
nix build --print-build-logs --out-link "$RUNNER_TEMP/sekrets-vm-driver" .#legacyPackages.x86_64-linux.sekrets-vm.driver
printf '| sekrets VM driver build | %ss |\\n' "$((SECONDS - start))" >> "$GITHUB_STEP_SUMMARY"`,
        },
        {
          name: 'Run the sekrets gateway test in the VM',
          id: 'sekrets-test',
          env: { ST_SEKRETS_BINARY: '${{ env.CI_SEKRETS_BINARY }}' },
          run: `start=$SECONDS
mkdir -p "$RUNNER_TEMP/sekrets-vm-out"
"$RUNNER_TEMP/sekrets-vm-driver/bin/nixos-test-driver" --output_directory "$RUNNER_TEMP/sekrets-vm-out"
printf '| sekrets VM test | %ss |\\n' "$((SECONDS - start))" >> "$GITHUB_STEP_SUMMARY"`,
        },
        { name: 'Save Nix outputs', if: `success() && env.CI_LOCAL_CACHES != '1' && ${optionalQueueCacheSave}`, run: 'bash scripts/ci-nix-cache save' },
        ...evidenceStepSteps('isolation-vm', ['isolation-vm', 'nixos-test-driver', 'transport-isolation-vm', 'sekrets-vm'], ['kvm', 'driver', 'vmtest', 'sekrets-driver', 'sekrets-test']),
      ],
    },
    // Not yet a required check: the ruleset gains `fractal-web` after both PR and merge-group
    // runs have emitted it successfully.
    // workspace-ci embeds scripts/ci-fractal-web-changes; its consumed-input list is authoritative.
    ...fractalWebJobs,
  }),
}, {"evidence-source": "Retains Git source objects only; builds nothing and has no signing key.", "pick-runner": "Runner selection uses live API state and builds nothing.", "upgrade-impact": "Runs Python/Git classification checks without downloads or compilation.", "namespace-capacity": "Capacity is live API state and builds nothing.", "linux-gate": "Collects completed checks and builds nothing.", "evidence-note-linux-gate": "Signs the aggregate gate result from workflow context; builds nothing.", "fractal-web-changes": "Diffs two fetched commits without blobs and builds nothing.", "fractal-web": "Collects the detection and execution results and builds nothing.", "evidence-gate-linux-gate": "Verifies Git notes only; builds nothing and never skips a check.", "evidence-gate-typescript-client": "Verifies Git notes only; builds nothing and never skips a check.", "evidence-gate-isolation-vm": "Verifies Git notes only; builds nothing and never skips a check.", "evidence-gate-genie-freshness": "Verifies Git notes only; builds nothing and never skips a check.", "evidence-gate-mail-redelivery-canaries": "Verifies Git notes only; builds nothing and never skips a check."}))))
