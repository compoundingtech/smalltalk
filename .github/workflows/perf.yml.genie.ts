import { defaultActionlintConfig, githubWorkflow } from '../../repos/effect-utils/genie/external.ts'
import { commonSetupSteps, linuxStageJob, perfStoresCache } from './workspace-ci.ts'

const baseline = '${{ runner.temp }}/perf-baseline'
const onMain = "success() && github.ref == 'refs/heads/main' && github.event_name != 'pull_request'"

// The load test is as long as linux-tests when its caches are warm and twice as long when its
// stores must generate, so it runs nightly on main and on pull requests that change the daemon or
// the store, never in Workspace CI (docs/ci.md, Performance gate).
export default githubWorkflow({
  name: 'Performance',
  on: {
    schedule: [{ cron: '23 2 * * *' }],
    pull_request: {
      paths: [
        'crates/smallclaims/**',
        'crates/st3/src/**',
        'crates/st3/tests/daemon_*.rs',
        'Cargo.lock',
        'scripts/ci-perf',
        '.github/workflows/perf.yml',
      ],
    },
    workflow_dispatch: {},
  },
  permissions: { contents: 'read' },
  concurrency: {
    group: 'perf-${{ github.event.pull_request.number || github.run_id }}',
    'cancel-in-progress': "${{ github.event_name == 'pull_request' }}",
  },
  actionlint: defaultActionlintConfig,
  jobs: {
    // A production-sized generated store under a busy host's request mix, compared with the worst
    // of main's last five reports. Main's successful runs add theirs.
    'perf-load': linuxStageJob({
      name: 'perf-load',
      stage: 'load',
      setup: commonSetupSteps,
      description: 'Run the load test',
      command: ['bash', 'scripts/ci-perf', 'load'],
      extraLogs: '${{ runner.temp }}/perf/',
      before: [
        perfStoresCache('load'),
        {
          name: "Restore main's load test reports",
          uses: 'actions/cache/restore@v4',
          with: {
            path: baseline,
            key: 'perf-load-baseline-${{ github.run_id }}',
            'restore-keys': 'perf-load-baseline-',
          },
        },
      ],
      after: [
        {
          name: "Keep this report among main's last five",
          if: onMain,
          run: `mkdir -p "${baseline}"
cp "$RUNNER_TEMP/perf/load.json" "${baseline}/load-\${{ github.run_id }}.json"
ls -1 "${baseline}"/load-*.json | sort -V -r | tail -n +6 | xargs -r rm --`,
        },
        {
          name: 'Save the reports',
          if: onMain,
          uses: 'actions/cache/save@v4',
          with: { path: baseline, key: 'perf-load-baseline-${{ github.run_id }}' },
        },
      ],
    }),
  },
})
