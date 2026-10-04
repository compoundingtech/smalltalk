import {
  defaultActionlintConfig,
  githubWorkflow,
  nixDevelopStep,
  plainFlakeJob,
  plainFlakeSetupSteps,
} from '../../repos/effect-utils/genie/external.ts'
import {
  afterPickRunner,
  buildEnv,
  commonSetupSteps,
  linuxRunner,
  linuxRunsOn,
  linuxStageJob as namespaceStageJob,
  linuxStageRunner,
  linuxStageRunsOn,
  perfStoresCache,
  pickRunnerJob,
  pickRunnerJobId,
  readOnlyBinaryCaches,
  workspacePreparationSteps,
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
  extraLogs = '',
}: {
  name: string
  stage: string
  setup: readonly unknown[]
  description?: string
  env?: Record<string, string>
  extraLogs?: string
}) => ({
  name,
  ...afterPickRunner,
  'runs-on': linuxStageRunsOn,
  'timeout-minutes': 120,
  defaults: { run: { shell: 'bash' } },
  env: { ...buildEnv, ...env },
  steps: [
    ...setup,
    {
      name: 'Summarize tested revision',
      run: `printf 'Checked merge/commit: \\x60%s\\x60 on %s CPUs, %s\\n\\n| Stage | Result | Elapsed | Exit |\\n| --- | --- | --- | --- |\\n' "$(git rev-parse HEAD)" "$(nproc)" "$(free -h | awk '/^Mem:/ {print $2 " memory"}')" >> "$GITHUB_STEP_SUMMARY"`,
    },
    nixDevelopStep({ name: description ?? 'Run nextest', command: ['bash', 'scripts/ci-linux', stage] }),
    {
      name: 'Save Nix outputs to the local Nix cache',
      if: "success() && env.CI_LOCAL_CACHES != '1'",
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

// Required gate. Label events belong to macos.yml so they never restart or cancel this workflow.
export default githubWorkflow({
  name: 'Workspace CI',
  on: {
    pull_request: {},
    // GitHub's merge queue runs the required checks on each queued entry; without this trigger the
    // queue never receives them and merges freeze.
    merge_group: {},
    push: { branches: ['main'] },
    workflow_dispatch: {},
  },
  permissions: { contents: 'read' },
  concurrency: {
    // PR updates replace stale checks; every other run has its own group so pending pushes survive.
    group: 'workspace-${{ github.event.pull_request.number || github.run_id }}-${{ github.event_name }}',
    'cancel-in-progress': '${{ github.event_name == \'pull_request\' }}',
  },
  // actionlint must know the Namespace shape label the stage jobs use.
  actionlint: {
    ...defaultActionlintConfig,
    selfHostedRunnerLabels: [...(defaultActionlintConfig.selfHostedRunnerLabels ?? []), ...linuxStageRunner],
  },
  jobs: {
    [pickRunnerJobId]: pickRunnerJob,
    // Namespace runners are already authenticated. Manual runs record the platform resource limits
    // alongside the CI workload so queue concurrency can be chosen from the actual account capacity.
    'namespace-capacity': {
      name: 'namespace-capacity',
      if: "github.event_name == 'workflow_dispatch'",
      'runs-on': linuxRunner,
      'timeout-minutes': 5,
      defaults: { run: { shell: 'bash' } },
      steps: [
        {
          name: 'Record Namespace platform capacity',
          run: `nsc workspace concurrency --output json | jq '{concurrency: [.concurrency[] | {platforms, limits, activeConcurrency}]}' > "$RUNNER_TEMP/namespace-capacity.json"
cat "$RUNNER_TEMP/namespace-capacity.json"
printf 'Measured at %s\\n\\n' "$(date -u +%FT%TZ)" >> "$GITHUB_STEP_SUMMARY"
printf '\\x60\\x60\\x60json\\n' >> "$GITHUB_STEP_SUMMARY"
cat "$RUNNER_TEMP/namespace-capacity.json" >> "$GITHUB_STEP_SUMMARY"
printf '\\n\\x60\\x60\\x60\\n' >> "$GITHUB_STEP_SUMMARY"`,
        },
        {
          name: 'Retain Namespace capacity evidence',
          uses: 'actions/upload-artifact@v4',
          with: {
            name: 'namespace-capacity',
            path: '${{ runner.temp }}/namespace-capacity.json',
            'if-no-files-found': 'error',
          },
        },
      ],
    },
    'genie-freshness': plainFlakeJob({
      name: 'genie-freshness',
      ...afterPickRunner,
      runsOn: linuxRunsOn,
      'timeout-minutes': 20,
      nix: { binaryCaches: readOnlyBinaryCaches },
      step: nixDevelopStep({ name: 'Check runner selection and generated files', flake: '.#genie', command: ['bash', '-c', 'python3 scripts/check-ci-runner-test && genie --check'] }),
    }),
    // Start non-required; apply the staged ruleset change only after this check passes on main.
    'typescript-client': {
      name: 'typescript-client',
      // Reuse freshness's slot so the five general ci1 runners can cover the initial fan-out.
      needs: ['pick-runner', 'genie-freshness'],
      if: "${{ !cancelled() && needs.genie-freshness.result == 'success' }}",
      'runs-on': linuxRunsOn,
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
          run: `lockfiles_hash=$(sha256sum apps/ios/package-lock.json clients/typescript/st3-client/package-lock.json | sha256sum | cut -d ' ' -f1)
printf 'hash=%s\\n' "$lockfiles_hash" >> "$GITHUB_OUTPUT"`,
        },
        {
          name: 'Cache the locked TypeScript and Effect toolchain',
          id: 'typescript-cache',
          uses: 'actions/cache@v5',
          with: {
            path: 'apps/ios/node_modules\nclients/typescript/st3-client/node_modules',
            key: 'typescript-client-${{ runner.os }}-node24.18.0-${{ steps.lockfiles.outputs.hash }}',
          },
        },
        {
          name: 'Install locked dependencies',
          if: "steps.typescript-cache.outputs.cache-hit != 'true'",
          run: 'npm ci --prefix apps/ios --ignore-scripts --no-audit --no-fund\nnpm ci --prefix clients/typescript/st3-client --ignore-scripts --no-audit --no-fund',
        },
        { name: 'Run client contracts, schemas and strict typechecks', run: 'bash scripts/ci-typescript-client' },
      ],
    },
    // The Linux gate runs as three jobs on separate runners, each with its own caches.
    // `linux-gate` below is the single required check that collects them.
    'linux-tests': linuxStageJob({
      name: 'linux-tests',
      stage: 'tests',
      setup: workspacePreparationSteps,
      // CI_RUN_ID keeps the messaging-fault evidence under target/messaging-faults and a failed
      // boot canary's evidence under target/boot-canaries.
      env: { CI_RUN_ID: '${{ github.run_id }}' },
      extraLogs: 'target/messaging-faults/\ntarget/boot-canaries/',
    }),
    'linux-clippy': linuxStageJob({
      name: 'linux-clippy',
      stage: 'clippy',
      setup: commonSetupSteps,
      description: 'Run clippy and the generated-client check',
    }),
    'linux-fleet-compat': linuxStageJob({
      name: 'linux-fleet-compat',
      stage: 'fleet-compat',
      setup: commonSetupSteps,
      description: 'Run fleet compatibility against the pinned older st3',
    }),
    'linux-gate': {
      name: 'linux-gate',
      needs: [pickRunnerJobId, 'linux-tests', 'linux-clippy', 'linux-fleet-compat'],
      // A skipped or cancelled stage must fail the gate, so it runs even when a stage failed.
      if: 'always()',
      'runs-on': linuxRunsOn,
      'timeout-minutes': 5,
      steps: [
        {
          name: 'Require every Linux stage to pass',
          // The stages only: pick-runner is skipped whenever ci1 is off.
          env: { RESULTS: '${{ needs.linux-tests.result }} ${{ needs.linux-clippy.result }} ${{ needs.linux-fleet-compat.result }}' },
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
      ...afterPickRunner,
      'runs-on': linuxRunsOn,
      'timeout-minutes': 60,
      defaults: { run: { shell: 'bash' } },
      env: buildEnv,
      steps: [
        { uses: 'actions/checkout@v4', with: { 'persist-credentials': false } },
        { name: 'Probe KVM', run: kvmProbe },
        ...plainFlakeSetupSteps({ nix: { binaryCaches: readOnlyBinaryCaches } }),
        {
          name: 'Archive the st2 integration test binary',
          run: `start=$SECONDS
nix develop -c cargo nextest archive --locked -p st2 --test integration --archive-file "$RUNNER_TEMP/isolation.tar.zst"
printf '| test archive build | %ss |\\n' "$((SECONDS - start))" >> "$GITHUB_STEP_SUMMARY"`,
        },
        {
          name: 'Build the NixOS VM test driver',
          run: `start=$SECONDS
nix build --print-build-logs --out-link "$RUNNER_TEMP/vm-driver" .#legacyPackages.x86_64-linux.transport-isolation-vm.driver
printf '| VM driver build | %ss |\\n' "$((SECONDS - start))" >> "$GITHUB_STEP_SUMMARY"`,
        },
        {
          name: 'Run all three systemd-scope tests in the VM',
          env: {
            ST_ISOLATION_ARCHIVE: '${{ runner.temp }}/isolation.tar.zst',
            ST_ISOLATION_WORKSPACE: '${{ github.workspace }}',
            ST_ISOLATION_TIMINGS: '${{ runner.temp }}/vm-timings.json',
          },
          run: `start=$SECONDS
mkdir -p "$RUNNER_TEMP/vm-out"
"$RUNNER_TEMP/vm-driver/bin/nixos-test-driver" --output_directory "$RUNNER_TEMP/vm-out"
jq -r '"| VM boot | \\(.boot_seconds)s |\\n| systemd-scope tests in VM (extract and run) | \\(.test_seconds)s |"' "$ST_ISOLATION_TIMINGS" >> "$GITHUB_STEP_SUMMARY"
printf '| VM test driver total | %ss |\\n' "$((SECONDS - start))" >> "$GITHUB_STEP_SUMMARY"`,
        },
      ],
    },
  },
})
