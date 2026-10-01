import {
  defaultActionlintConfig,
  githubWorkflow,
  nixDevelopStep,
  plainFlakeJob,
} from '../../repos/effect-utils/genie/external.ts'
import { buildEnv, linuxRunner, readOnlyBinaryCaches, workspacePreparationSteps } from './workspace-ci.ts'

// Required gate. Label events belong to macos.yml so they never restart or cancel this workflow.
export default githubWorkflow({
  name: 'Workspace CI',
  on: {
    pull_request: {},
    push: { branches: ['main'] },
    // Daily complete run of the st2 catalog/supervisor suite on main.
    schedule: [{ cron: '23 4 * * *' }],
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
          name: 'Summarize tested revision and selection',
          env: { REASON: '${{ steps.st2.outputs.reason }}' },
          run: `printf 'Checked merge/commit: \\x60%s\\x60 on %s CPUs, %s\\n\\n%s\\n\\n| Stage | Result | Elapsed | Exit |\\n| --- | --- | --- | --- |\\n' "$(git rev-parse HEAD)" "$(nproc)" "$(free -h | awk '/^Mem:/ {print $2 " memory"}')" "$REASON" >> "$GITHUB_STEP_SUMMARY"`,
        },
        {
          ...nixDevelopStep({ name: 'Run nextest, clippy, generated clients and fleet compatibility in parallel', command: ['bash', 'scripts/ci-linux'] }),
          env: { ST2_FILTER: '${{ steps.st2.outputs.filter }}' },
        },
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
  },
})
