import { defaultActionlintConfig, githubWorkflow, nixDevelopStep, plainFlakeSetupSteps } from '../../repos/effect-utils/genie/external.ts'
import { buildEnv, linuxRunner, linuxStageRunner, readOnlyBinaryCaches } from './workspace-ci.ts'

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
    workflow_dispatch: {
      inputs: {
        study_run: { description: 'Exact governed mission run created before the study dispatch', type: 'string', default: '' },
        collections_study: {
          description: 'Run the frozen shared collections-v1 study (B1,C1,C2,B2) instead of ordinary perf-load',
          type: 'boolean', default: false,
        },
      },
    },
  },
  permissions: { contents: 'read', actions: 'read', 'pull-requests': 'read' },
  concurrency: {
    // Preserve the existing PR group; replace obsolete main pushes, never pinned controls.
    group: "perf-${{ github.event.pull_request.number || (github.event_name == 'push' && github.ref == 'refs/heads/main' && 'main') || github.run_id }}",
    'cancel-in-progress': "${{ github.event_name == 'pull_request' || (github.event_name == 'push' && github.ref == 'refs/heads/main') }}",
  },
  actionlint: {
    ...defaultActionlintConfig,
    selfHostedRunnerLabels: [...(defaultActionlintConfig.selfHostedRunnerLabels ?? []), ...linuxRunner, ...linuxStageRunner],
  },
  jobs: {
    'collections-study': {
      name: 'shared collections-v1 study (four executions)',
      if: "github.event_name == 'workflow_dispatch' && inputs.collections_study == true",
      'runs-on': linuxStageRunner,
      'timeout-minutes': 240,
      defaults: { run: { shell: 'bash' } },
      env: { ...buildEnv, SCCACHE_IDLE_TIMEOUT: '0', COLLECTIONS_STUDY_RUN: '${{ inputs.study_run }}' },
      steps: [
        { uses: 'actions/checkout@v4', with: { 'fetch-depth': 0, 'persist-credentials': false } },
        { name: 'Check finite study controls', run: 'python3 scripts/ci-collections-study-test' },
        { name: 'Freeze compatible study sources and observation overlay', run: 'python3 scripts/ci-collections-study prepare --out "$RUNNER_TEMP/collections-study"' },
        ...plainFlakeSetupSteps({ nix: { binaryCaches: readOnlyBinaryCaches } }),
        {
          name: 'Isolate study state and preserve the normal compiler cache',
          run: `study_home="$RUNNER_TEMP/collections-study-home"
mkdir -p "$study_home"/{.config,.cache,.local/state} "$RUNNER_TEMP/collections-cargo-home"/{registry,git} "$RUNNER_TEMP/collections-sccache"
printf 'HOME=%s\\nXDG_CONFIG_HOME=%s/.config\\nXDG_CACHE_HOME=%s/.cache\\nXDG_STATE_HOME=%s/.local/state\\n' "$study_home" "$study_home" "$study_home" "$study_home" >> "$GITHUB_ENV"
printf 'CARGO_HOME=%s\\nSCCACHE_DIR=%s\\nSCCACHE_CACHE_SIZE=1G\\n' "$RUNNER_TEMP/collections-cargo-home" "$RUNNER_TEMP/collections-sccache" >> "$GITHUB_ENV"`,
        },
        {
          name: 'Build both frozen release artifacts, then B1 C1 C2 B2 without retries',
          run: 'nix develop "$RUNNER_TEMP/collections-study/B#perf" -c python3 "$GITHUB_WORKSPACE/scripts/ci-collections-study" run --out "$RUNNER_TEMP/collections-study"',
        },
        {
          name: 'Retain all study source, raw reports, input hashes and failures',
          uses: 'actions/upload-artifact@v4', if: 'always()',
          with: { name: 'shared-collections-v1-evidence', path: '${{ runner.temp }}/collections-study/\n!${{ runner.temp }}/collections-study/target-*/\n!${{ runner.temp }}/collections-study/B/\n!${{ runner.temp }}/collections-study/C/\n!${{ runner.temp }}/collections-study/standard-generated-inputs/',
            'retention-days': 30, 'if-no-files-found': 'error' },
        },
      ],
    },
    'perf-load': {
      name: 'perf-load',
      if: "github.event_name != 'workflow_dispatch' || inputs.collections_study != true",
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
