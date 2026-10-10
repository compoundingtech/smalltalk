import { defaultActionlintConfig, githubWorkflow, nixDevelopStep, plainFlakeSetupSteps } from '../../repos/effect-utils/genie/external.ts'
import { buildEnv, linuxRunner, linuxStageRunner, readOnlyBinaryCaches } from './workspace-ci.ts'

const diagnosticRunner = ['nscloud-ubuntu-24.04-amd64-8x16-with-features;job.priority=2', 'namespace-features:github.run-id=${{ github.run_id }}']

// Guard BEFORE each external action; charge minute-granular timeouts to the
// immutable setup deadline, with 60 seconds reserved for action dispatch/return.
const setupAdmission = String.raw`python3 - <<'PYSETUP'
import json, os, time, math, hashlib
from pathlib import Path
r = json.loads(Path(os.environ['LOG_DIET_DEADLINE']).read_text())
keys = ('ST_AGENT','ST3_SUBJECT','GITHUB_ACTIONS','GITHUB_EVENT_NAME','GITHUB_RUN_ID','GITHUB_RUN_ATTEMPT','GITHUB_JOB','RUNNER_NAME')
identity = {k: os.environ.get(k) for k in keys}
now = time.monotonic()
if r.get('identity') != identity or r.get('boot_sha256') != hashlib.sha256(Path('/proc/sys/kernel/random/boot_id').read_bytes()).hexdigest():
    raise SystemExit('setup identity mismatch')
for key in ('start','setup_end','cutoff','upload_end'):
    v = r.get(key)
    if isinstance(v, bool) or not isinstance(v, (int,float)) or not math.isfinite(v) or v < 0:
        raise SystemExit('malformed setup deadline')
if (r['setup_end'],r['cutoff'],r['upload_end']) != (r['start']+600,r['start']+6000,r['start']+6900) or not r['start'] <= now < r['setup_end']:
    raise SystemExit('expired/future/reset setup deadline')
minutes = int((r['setup_end'] - now - 60) // 60)
if minutes < 1:
    raise SystemExit('insufficient setup action budget')
with open(os.environ['GITHUB_OUTPUT'],'a') as f:
    f.write('minutes=' + str(minutes) + '\n')
PYSETUP`

const boundedSetupSteps = [
  { uses: 'actions/checkout@v4', with: { 'persist-credentials': false } },
  ...plainFlakeSetupSteps({ nix: { binaryCaches: readOnlyBinaryCaches } }),
].flatMap((step, index) => {
  const id = `admit_setup_${index}`
  return [
    { id, name: `Admit setup action ${index} within the first-step deadline`, 'timeout-minutes': 1, run: setupAdmission },
    { ...step, if: `success() && steps.${id}.outputs.minutes != ''`, 'timeout-minutes': '$' + '{{ fromJSON(steps.' + id + '.outputs.minutes) }}' },
  ]
})

const snapshotAttempt = "!cancelled() && (github.event_name == 'pull_request' || github.ref == 'refs/heads/main') && (steps.load.outcome == 'success' || steps.load.outcome == 'failure')"
const snapshotPublished = "!cancelled() && steps.cache.outcome == 'success' && steps.cache.outputs.publish == 'true'"
const paths = [
  'crates/smallclaims/**',
  'crates/st3/src/**',
  'crates/st3/tests/daemon_*.rs',
  'crates/st3/tests/perf_load.rs',
  'crates/st3/Cargo.toml',
  'Cargo.toml',
  'Cargo.lock',
  '.cargo/config.toml',
  'flake.nix',
  'flake.lock',
  'docs/st3/schema.md',
  'scripts/ci-perf*',
  'scripts/ci-nix-cache',
  '.github/workflows/perf.yml',
]

// Performance always stays on Namespace. Main's successful runs seed durable snapshots and
// real baseline reports. Each PR can also reuse its own snapshots, outside the dependency-cache pool.
export default githubWorkflow({
  name: 'Performance',
  on: {
    push: { branches: ['main'], paths },
    schedule: [{ cron: '23 2 * * *' }],
    pull_request: { paths },
    workflow_dispatch: { inputs: { log_diet_assignment: { description: 'Immutable separately approved three-source diagnostic assignment', required: false, type: 'string', default: '' } } },
  },
  permissions: { contents: 'read', actions: 'read', 'pull-requests': 'read' },
  concurrency: {
    // Preserve the existing PR group; replace obsolete main pushes, never pinned controls.
    group: "perf-${{ github.event.pull_request.number || (github.event_name == 'push' && github.ref == 'refs/heads/main' && 'main') || github.run_id }}",
    'cancel-in-progress': "${{ github.event_name == 'pull_request' || (github.event_name == 'push' && github.ref == 'refs/heads/main') }}",
  },
  actionlint: {
    ...defaultActionlintConfig,
    selfHostedRunnerLabels: [...(defaultActionlintConfig.selfHostedRunnerLabels ?? []), ...linuxRunner, ...linuxStageRunner, ...diagnosticRunner],
  },
  jobs: {
    'log-diet-three-source': {
      if: "${{ github.event_name == 'workflow_dispatch' && inputs.log_diet_assignment != '' }}",
      name: 'log-diet-three-source',
      'runs-on': diagnosticRunner,
      'timeout-minutes': 120,
      defaults: { run: { shell: 'bash' } },
      env: { ...buildEnv, LOG_DIET_ASSIGNMENT: '${{ inputs.log_diet_assignment }}' },
      steps: [
        {
          name: 'Bind the first-step absolute deadline before checkout or installation',
          run: "python3 - <<'PYBOOT'\nimport json, os, time, hashlib\nfrom pathlib import Path\nstart = time.monotonic()\nroot = Path(os.environ['RUNNER_TEMP']) / 'log-diet-three-source'\nroot.mkdir()\nkeys = ('ST_AGENT','ST3_SUBJECT','GITHUB_ACTIONS','GITHUB_EVENT_NAME','GITHUB_RUN_ID','GITHUB_RUN_ATTEMPT','GITHUB_JOB','RUNNER_NAME')\nidentity = {k: os.environ.get(k) for k in keys}\nrecord = {'identity': identity, 'start': start, 'cutoff': start + 6000, 'setup_end': start + 600, 'upload_end': start + 6900, 'boot_sha256': hashlib.sha256(Path('/proc/sys/kernel/random/boot_id').read_bytes()).hexdigest()}\nwith (root / 'deadline.json').open('x') as f: json.dump(record, f)\nwith open(os.environ['GITHUB_ENV'], 'a') as f:\n    f.write('LOG_DIET_ROOT=' + str(root) + '\\nLOG_DIET_DEADLINE=' + str(root / 'deadline.json') + '\\n')\nPYBOOT",
        },
        ...boundedSetupSteps,
        {
          name: 'Full generation, actionlint and three original cases under shared deadlines',
          run: 'python3 scripts/ci-log-diet-study.py study --repo "$GITHUB_WORKSPACE" --root "$LOG_DIET_ROOT" --deadline "$LOG_DIET_DEADLINE" --assignment "$LOG_DIET_ASSIGNMENT"',
        },
        {
          id: 'retain',
          name: 'Retain negative and incomplete outcomes before upload',
          if: 'always()',
          run: 'python3 scripts/ci-log-diet-study.py retain --root "$LOG_DIET_ROOT" --deadline "$LOG_DIET_DEADLINE"',
        },
        {
          name: 'Retain original diagnostics and actual tool, feature, binary and cleanup receipts',
          if: "always() && steps.retain.outputs.upload_minutes != ''",
          uses: 'actions/upload-artifact@v4',
          'timeout-minutes': '${{ fromJSON(steps.retain.outputs.upload_minutes) }}',
          with: {
            name: 'log-diet-three-source-${{ github.run_attempt }}',
            path: '${{ runner.temp }}/log-diet-three-source/\n!${{ runner.temp }}/log-diet-three-source/sources/',
            'compression-level': 0, 'retention-days': 30, 'if-no-files-found': 'error',
          },
        },
      ],
    },
    'perf-load': {
      if: "${{ github.event_name != 'workflow_dispatch' || inputs.log_diet_assignment == '' }}",
      name: 'perf-load',
      'runs-on': linuxStageRunner,
      'timeout-minutes': 30,
      defaults: { run: { shell: 'bash' } },
      env: {
        ...buildEnv,
        SCCACHE_IDLE_TIMEOUT: '0',
        PERF_PR_NUMBER: '${{ github.event.pull_request.number }}',
        PERF_HEAD_REPOSITORY: '${{ github.event.pull_request.head.repo.full_name }}',
      },
      steps: [
        { uses: 'actions/checkout@v4', with: { 'fetch-depth': 0, 'persist-credentials': false } },
        {
          name: 'Name compatible snapshot recipes',
          env: {
            PERF_BUILD_SNAPSHOT: "perf-load-build-v1-${{ hashFiles('Cargo.lock', 'flake.lock', 'flake.nix', 'Cargo.toml', 'crates/st3/Cargo.toml', '.cargo/config.toml') }}",
            PERF_STORES_SNAPSHOT: "perf-load-stores-v1-${{ hashFiles('crates/st3/tests/daemon_bench.rs', 'docs/st3/schema.md') }}",
          },
          run: 'printf "PERF_BUILD_SNAPSHOT=%s\\nPERF_STORES_SNAPSHOT=%s\\n" "$PERF_BUILD_SNAPSHOT" "$PERF_STORES_SNAPSHOT" >> "$GITHUB_ENV"',
        },
        {
          name: 'Restore compatible build, Nix, stores and main baseline snapshots',
          env: { GH_TOKEN: '${{ github.token }}' },
          run: 'python3 scripts/ci-perf-cache restore',
        },
        ...plainFlakeSetupSteps({ nix: { binaryCaches: readOnlyBinaryCaches } }),
        {
          name: 'Isolate test state and keep the compiler cache',
          run: `home="$RUNNER_TEMP/test-home"
mkdir -p "$home"/{.config,.cache,.local/state} "$RUNNER_TEMP/cargo-home"/{registry,git} "$RUNNER_TEMP/perf-sccache"
printf 'CARGO_HOME=%s\\nCI_CACHE_DIR=%s\\nSCCACHE_DIR=%s\\nSCCACHE_CACHE_SIZE=1G\\n' "$RUNNER_TEMP/cargo-home" "$RUNNER_TEMP/st-ci-cache" "$RUNNER_TEMP/perf-sccache" >> "$GITHUB_ENV"
printf 'HOME=%s\\nXDG_CONFIG_HOME=%s/.config\\nXDG_CACHE_HOME=%s/.cache\\nXDG_STATE_HOME=%s/.local/state\\n' "$home" "$home" "$home" "$home" >> "$GITHUB_ENV"`,
        },
        { name: 'Use the cached Nix outputs', run: 'bash scripts/ci-nix-cache use' },
        {
          name: 'Prepare fast scratch space for a new store recipe',
          // The generator already prefers RAM when enough is available. Its source stores are
          // copied to RUNNER_TEMP before measuring: measurements still use the normal disk.
          run: 'if [ ! -s "$RUNNER_TEMP/st-bench/generated-1.sqlite3" ]; then sudo mount -o remount,size=10G /dev/shm; fi',
        },
        { ...nixDevelopStep({ name: 'Run the release load test', flake: '.#perf', command: ['bash', 'scripts/ci-perf', 'load'] }), id: 'load' },
        {
          id: 'cache',
          name: 'Save build and Nix snapshots',
          if: snapshotAttempt,
          run: `if ! python3 scripts/ci-perf-cache check-report; then exit 0; fi
nix print-dev-env .#perf --profile "$RUNNER_TEMP/perf-shell" > /dev/null
nix develop .#perf -c env TMPDIR="$RUNNER_TEMP" sccache --show-stats || echo "::warning::sccache statistics unavailable; retaining the successful build"
CI_PERF_SHELL_ROOT="$RUNNER_TEMP/perf-shell" bash scripts/ci-nix-cache save
python3 scripts/ci-perf-cache pack`,
        },
        {
          name: 'Retain the build and Nix cache',
          if: snapshotPublished,
          uses: 'actions/upload-artifact@v4',
          with: { name: '${{ env.PERF_BUILD_SNAPSHOT }}', path: '${{ runner.temp }}/perf-snapshots/build.tar.zst', 'compression-level': 0, 'retention-days': 7, 'if-no-files-found': 'error', overwrite: true },
        },
        {
          name: 'Retain the generated stores',
          if: snapshotPublished,
          uses: 'actions/upload-artifact@v4',
          with: { name: '${{ env.PERF_STORES_SNAPSHOT }}', path: '${{ runner.temp }}/perf-snapshots/stores.tar.zst', 'compression-level': 0, 'retention-days': 7, 'if-no-files-found': 'error', overwrite: true },
        },
        {
          name: 'Retain the load report, log and timing',
          uses: 'actions/upload-artifact@v4',
          if: 'always()',
          with: { name: 'perf-load-logs', path: '${{ runner.temp }}/perf/', 'retention-days': 30, 'if-no-files-found': 'ignore', overwrite: true },
        },
      ],
    },
  },
})
