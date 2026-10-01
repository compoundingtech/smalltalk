import {
  defaultActionlintConfig,
  githubWorkflow,
  nixDevelopStep,
  plainFlakeJob,
  plainFlakeSetupSteps,
} from '../../repos/effect-utils/genie/external.ts'
import { buildEnv, linuxRunner, readOnlyBinaryCaches, workspacePreparationSteps } from './workspace-ci.ts'

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

// Required gate. Label events belong to macos.yml so they never restart or cancel this workflow.
export default githubWorkflow({
  name: 'Workspace CI',
  on: {
    pull_request: {},
    push: { branches: ['main'] },
    workflow_dispatch: {},
  },
  permissions: { contents: 'read' },
  concurrency: {
    group: 'workspace-${{ github.event.pull_request.number || github.ref }}-${{ github.event_name }}',
    'cancel-in-progress': '${{ github.event_name == \'pull_request\' }}',
  },
  actionlint: defaultActionlintConfig,
  jobs: {
    'genie-freshness': plainFlakeJob({
      name: 'genie-freshness',
      runsOn: linuxRunner,
      'timeout-minutes': 20,
      nix: { binaryCaches: readOnlyBinaryCaches },
      step: nixDevelopStep({ name: 'Check generated files', flake: '.#genie', command: ['genie', '--check'] }),
    }),
    'linux-gate': {
      name: 'linux-gate',
      'runs-on': linuxRunner,
      'timeout-minutes': 120,
      defaults: { run: { shell: 'bash' } },
      // CI_RUN_ID keeps the messaging-fault evidence under target/messaging-faults.
      env: { ...buildEnv, CI_RUN_ID: '${{ github.run_id }}' },
      steps: [
        ...workspacePreparationSteps,
        {
          name: 'Summarize tested revision',
          run: `printf 'Checked merge/commit: \\x60%s\\x60 on %s CPUs, %s\\n\\n| Stage | Result | Elapsed | Exit |\\n| --- | --- | --- | --- |\\n' "$(git rev-parse HEAD)" "$(nproc)" "$(free -h | awk '/^Mem:/ {print $2 " memory"}')" >> "$GITHUB_STEP_SUMMARY"`,
        },
        nixDevelopStep({ name: 'Run nextest, clippy, generated clients and fleet compatibility in parallel', command: ['bash', 'scripts/ci-linux'] }),
        {
          name: 'Retain stage logs, timings and messaging-fault evidence',
          uses: 'actions/upload-artifact@v4',
          if: 'always()',
          with: {
            name: 'linux-ci-logs',
            path: '${{ runner.temp }}/ci-logs/\ntarget/messaging-faults/',
            'if-no-files-found': 'ignore',
          },
        },
      ],
    },
    // st2's transport-isolation cascade tests need a real systemd user manager, which the
    // runner image lacks. A NixOS VM runs this job's prebuilt test binary; it compiles nothing.
    'isolation-vm': {
      name: 'isolation-vm',
      'runs-on': linuxRunner,
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
          name: 'Run the exec and pty cascade tests in the VM',
          env: {
            ST_ISOLATION_ARCHIVE: '${{ runner.temp }}/isolation.tar.zst',
            ST_ISOLATION_WORKSPACE: '${{ github.workspace }}',
            ST_ISOLATION_TIMINGS: '${{ runner.temp }}/vm-timings.json',
          },
          run: `start=$SECONDS
"$RUNNER_TEMP/vm-driver/bin/nixos-test-driver" --output_directory "$RUNNER_TEMP/vm-out"
jq -r '"| VM boot | \\(.boot_seconds)s |\\n| cascade tests in VM (extract and run) | \\(.test_seconds)s |"' "$ST_ISOLATION_TIMINGS" >> "$GITHUB_STEP_SUMMARY"
printf '| VM test driver total | %ss |\\n' "$((SECONDS - start))" >> "$GITHUB_STEP_SUMMARY"`,
        },
      ],
    },
  },
})
