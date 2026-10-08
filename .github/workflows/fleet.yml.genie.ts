import { buildSnapshotSave, optionalQueueCacheSave } from './build-snapshot.ts'
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
  linuxRunner,
  linuxStageJob as namespaceStageJob,
  linuxStageRunner,
  linuxStageRunsOn,
  perfStoresCache,
  mailStageRunsOn,
  pickRunnerJob,
  pickRunnerJobId,
  supportingLinuxRunsOn,
  supportingStageRunsOn,
  secondaryStageRunsOn,
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
  ],
})

// Required gate. Label events belong to macos.yml so they never restart or cancel this workflow.
export default githubWorkflow(auditCaches({
  name: 'Workspace CI',
  on: {
    pull_request: {},
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
  jobs: {
    [pickRunnerJobId]: pickRunnerJob,
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
    // Manual runs retain the existing Namespace capacity report.
    'namespace-capacity': {
      name: 'namespace-capacity',
      if: "github.event_name == 'workflow_dispatch' || github.event_name == 'merge_group'",
      'runs-on': `\${{ fromJSON(github.event_name == 'merge_group' && '["ubuntu-latest"]' || format('${JSON.stringify(linuxRunner).replaceAll('${{ github.run_id }}', '{0}')}', github.run_id)) }}`,
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
        {
          name: 'Record Namespace platform capacity',
          if: "github.event_name == 'workflow_dispatch'",
          run: `nsc workspace concurrency --output json | jq '{concurrency: [.concurrency[] | {platforms, limits, activeConcurrency}]}' > "$RUNNER_TEMP/namespace-capacity.json"
cat "$RUNNER_TEMP/namespace-capacity.json"
printf 'Measured at %s\\n\\n' "$(date -u +%FT%TZ)" >> "$GITHUB_STEP_SUMMARY"
printf '\\x60\\x60\\x60json\\n' >> "$GITHUB_STEP_SUMMARY"
cat "$RUNNER_TEMP/namespace-capacity.json" >> "$GITHUB_STEP_SUMMARY"
printf '\\n\\x60\\x60\\x60\\n' >> "$GITHUB_STEP_SUMMARY"`,
        },
        {
          name: 'Retain Namespace capacity evidence',
          if: "github.event_name == 'workflow_dispatch'",
          uses: 'actions/upload-artifact@v4',
          with: {
            name: 'namespace-capacity',
            path: '${{ runner.temp }}/namespace-capacity.json',
            'if-no-files-found': 'error',
          },
        },
      ],
    },
    'genie-freshness': {
      name: 'genie-freshness',
      env: { CI_CACHE_DEV_SHELL: 'genie' },
      ...afterPickRunner,
      'runs-on': supportingLinuxRunsOn,
      'timeout-minutes': 20,
      steps: [
        ...commonSetupSteps.filter((step) => !('id' in step && step.id === 'cargo-cache')),
        nixDevelopStep({ name: 'Check runner selection and generated files', flake: '.#genie', command: ['bash', '-c', 'python3 scripts/check-ci-runner-test && python3 scripts/ci-mail-redelivery-canaries-test && python3 scripts/ci-test-partitions-test && python3 scripts/ci-test-archive-test && python3 scripts/check-ci-test-paths && python3 scripts/ci-queue-watch-test && python3 scripts/check-main-ci-test && python3 scripts/ci-perf-cache-test && python3 scripts/ci-cache-audit-test && genie --check'] }),
        { name: 'Save Nix outputs', if: `success() && env.CI_LOCAL_CACHES != '1' && ${optionalQueueCacheSave}`, run: 'bash scripts/ci-nix-cache save' },
        ...buildSnapshotSave,
      ],
    },
    // Check the shared client and its iOS consumer before merge.
    'typescript-client': {
      name: 'typescript-client',
      // Keep generated-file validation ahead of its consumers.
      needs: ['pick-runner', 'genie-freshness'],
      if: "${{ !cancelled() && needs.genie-freshness.result == 'success' }}",
      'runs-on': supportingLinuxRunsOn,
      'timeout-minutes': 10,
      defaults: { run: { shell: 'bash' } },
      steps: [
        { uses: 'actions/checkout@v4', with: { 'persist-credentials': false } },
        // Node 24, as in the workspace shell; schema tests use its native TypeScript loading.
        { uses: 'actions/setup-node@v4', with: { 'node-version': '24.18.0' } },
        {
          name: 'Fingerprint the locked dependencies',
          id: 'lockfiles',
          // ci1's Nix runner lacks the Node 20 helper used by GitHub's hashFiles expression.
          run: `lockfiles_hash=$(sha256sum apps/ios/package-lock.json clients/typescript/st3-client/package-lock.json clients/typescript/st3-views/package-lock.json | sha256sum | cut -d ' ' -f1)
printf 'hash=%s\\n' "$lockfiles_hash" >> "$GITHUB_OUTPUT"`,
        },
        {
          name: 'Cache the locked TypeScript and Effect toolchain',
          id: 'typescript-cache',
          uses: 'actions/cache@v5',
          with: {
            path: 'apps/ios/node_modules\nclients/typescript/st3-client/node_modules\nclients/typescript/st3-views/node_modules',
            key: 'typescript-client-${{ runner.os }}-node24.18.0-${{ steps.lockfiles.outputs.hash }}',
          },
        },
        {
          name: 'Install locked client dependencies',
          if: "steps.typescript-cache.outputs.cache-hit != 'true'",
          run: 'npm ci --prefix clients/typescript/st3-client --ignore-scripts --no-audit --no-fund',
        },
        {
          name: 'Run client contracts, schemas and strict typechecks',
          run: 'npm test --prefix clients/typescript/st3-client\nnpm run typecheck --prefix clients/typescript/st3-client',
        },
        {
          name: 'Install locked view dependencies',
          if: "steps.typescript-cache.outputs.cache-hit != 'true'",
          run: 'npm ci --prefix clients/typescript/st3-views --ignore-scripts --no-audit --no-fund',
        },
        {
          name: 'Install locked iOS dependencies',
          if: "steps.typescript-cache.outputs.cache-hit != 'true'",
          run: 'npm ci --prefix apps/ios --ignore-scripts --no-audit --no-fund',
        },
        { name: 'Check shared views, fixtures and iOS consumers', run: 'npm test --prefix clients/typescript/st3-views\nnpm run typecheck --prefix clients/typescript/st3-views\napps/ios/node_modules/.bin/tsc --noEmit -p apps/ios\nnpm test --prefix apps/ios' },
      ],
    },
    'linux-test-build': {
      name: 'linux-test-build',
      ...afterPickRunner,
      'runs-on': linuxStageRunsOn,
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
          command: ['python3', 'scripts/ci-test-archive', 'build'] }), id: 'archive' },
        {
          name: 'Publish this run attempt’s exact-source test archives', id: 'upload',
          uses: 'actions/upload-artifact@v4',
          with: { name: 'linux-test-archives-${{ github.run_attempt }}',
            path: '${{ runner.temp }}/ci-test-archives', 'if-no-files-found': 'error',
            'compression-level': 0, 'retention-days': 3 },
        },
        ...buildSnapshotSave,
      ],
    },
    // Two test partitions and the two supporting stages retain independent CPU capacity.
    // `linux-gate` below is the single required check that collects them.
    'linux-tests': {
      ...linuxStageJob({
        name: 'linux-tests',
        stage: 'tests',
        setup: testArchiveConsumerSetup,
        runsOn: linuxStageRunsOn,
        // CI_RUN_ID keeps the messaging-fault evidence under target/messaging-faults and a failed
        // boot canary's evidence under target/boot-canaries.
        env: { CI_RUN_ID: '${{ github.run_id }}', CI_TEST_PARTITION: 'hash:1/2', CI_TEST_THREADS: '8' },
        extraLogs: 'target/messaging-faults/\ntarget/boot-canaries/',
        before: [nixDevelopStep({ name: 'Prove both shards cover every selected test', command: ['python3', 'scripts/ci-test-partitions'] })],
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
        env: { CI_RUN_ID: '${{ github.run_id }}', CI_TEST_PARTITION: 'hash:2/2', CI_TEST_THREADS: '8' },
        extraLogs: 'target/messaging-faults/\ntarget/boot-canaries/',
      }),
      needs: [pickRunnerJobId, 'linux-test-build'],
      if: "${{ !cancelled() }}",
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
        env: { CI_RUN_ID: '${{ github.run_id }}', CI_TEST_THREADS: '8' },
        extraLogs: 'target/messaging-faults/\ntarget/boot-canaries/',
      }),
      needs: [pickRunnerJobId, 'linux-test-build'],
      if: "${{ !cancelled() }}",
    },
    'linux-gate': {
      name: 'linux-gate',
      needs: [pickRunnerJobId, 'linux-test-build', 'upgrade-impact', 'linux-tests', 'linux-tests-shard-2', 'linux-clippy', 'linux-fleet-compat', 'mail-redelivery-canaries'],
      // A skipped or cancelled stage must fail the gate, so it runs even when a stage failed.
      if: 'always()',
      // Aggregation needs no build caches and must not queue behind the work it summarizes.
      'runs-on': 'ubuntu-latest',
      'timeout-minutes': 5,
      steps: [
        {
          name: 'Require every Linux stage to pass',
          // The stages only: pick-runner is skipped whenever ci1 is off.
          env: { RESULTS: '${{ needs.linux-test-build.result }} ${{ needs.upgrade-impact.result }} ${{ needs.linux-tests.result }} ${{ needs.linux-tests-shard-2.result }} ${{ needs.linux-clippy.result }} ${{ needs.linux-fleet-compat.result }} ${{ needs.mail-redelivery-canaries.result }}' },
          run: `echo "stage results: $RESULTS"
for result in $RESULTS; do
  [ "$result" = success ] || exit 1
done`,
        },
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
      defaults: { run: { shell: 'bash' } },
      env: { ...buildEnv, CI_CACHE_DEV_SHELL: 'default' },
      steps: [
        ...testArchiveConsumerSetup,
        { name: 'Probe KVM', run: kvmProbe },
        {
          name: 'Build the NixOS VM test driver',
          run: `start=$SECONDS
nix build --print-build-logs --out-link "$RUNNER_TEMP/vm-driver" .#legacyPackages.x86_64-linux.transport-isolation-vm.driver
printf '| VM driver build | %ss |\\n' "$((SECONDS - start))" >> "$GITHUB_STEP_SUMMARY"`,
        },
        {
          name: 'Run all three systemd-scope tests in the VM',
          env: {
            ST_ISOLATION_ARCHIVE: '${{ runner.temp }}/ci-test-archives/st2.tar.zst',
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
          run: `start=$SECONDS
nix build --print-build-logs --out-link "$RUNNER_TEMP/sekrets-vm-driver" .#legacyPackages.x86_64-linux.sekrets-vm.driver
printf '| sekrets VM driver build | %ss |\\n' "$((SECONDS - start))" >> "$GITHUB_STEP_SUMMARY"`,
        },
        {
          name: 'Run the sekrets gateway test in the VM',
          env: { ST_SEKRETS_BINARY: '${{ env.CI_SEKRETS_BINARY }}' },
          run: `start=$SECONDS
mkdir -p "$RUNNER_TEMP/sekrets-vm-out"
"$RUNNER_TEMP/sekrets-vm-driver/bin/nixos-test-driver" --output_directory "$RUNNER_TEMP/sekrets-vm-out"
printf '| sekrets VM test | %ss |\\n' "$((SECONDS - start))" >> "$GITHUB_STEP_SUMMARY"`,
        },
        { name: 'Save Nix outputs', if: `success() && env.CI_LOCAL_CACHES != '1' && ${optionalQueueCacheSave}`, run: 'bash scripts/ci-nix-cache save' },
      ],
    },
  },
}, {"pick-runner": "Runner selection uses live API state and builds nothing.", "upgrade-impact": "Runs Python/Git classification checks without downloads or compilation.", "namespace-capacity": "Capacity is live API state and builds nothing.", "linux-gate": "Collects completed checks and builds nothing."}))
