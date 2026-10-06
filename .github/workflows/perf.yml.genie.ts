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
    workflow_dispatch: {},
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
    'perf-load': {
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
nix develop .#perf -c env TMPDIR="$RUNNER_TEMP" sccache --show-stats
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
