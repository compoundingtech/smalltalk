import { defaultActionlintConfig, githubWorkflow } from '../../repos/effect-utils/genie/external.ts'
import { auditCaches } from './cache-audit.ts'

// The label of the runners that hold the protected observer and its operator configuration.
// It names no pull-request pool: this workflow starts only from a finished merge-group run of
// the default branch's own workflow file, never from a pull request's code.
export const evidenceRunner = 'evidence-ci'
// A second label, equal to the runner group's name: the job needs a runner with both, so a stray
// `evidence-ci` label elsewhere cannot attract it. The generator's validation accepts string labels
// only (no `{group, labels}` form); the group's own restriction to this workflow file at refs/heads/main
// is set on the runner group by the operator.
export const evidenceGroup = 'evidence-observers'

export default githubWorkflow(auditCaches({
  name: 'Evidence observe',
  on: {
    workflow_run: { workflows: ['Workspace CI'], types: ['completed'] },
    workflow_dispatch: {
      inputs: {
        run_id: { description: 'Workspace CI run to observe again (a merge-group run)', required: true, type: 'string' },
        attempt: { description: 'Run attempt', required: true, default: '1', type: 'string' },
      },
    },
  },
  permissions: { contents: 'write', actions: 'read' },
  actionlint: {
    ...defaultActionlintConfig,
    selfHostedRunnerLabels: [...(defaultActionlintConfig.selfHostedRunnerLabels ?? []), evidenceGroup, evidenceRunner],
  },
  // One observation at a time: the scratch repository persists between runs.
  concurrency: { group: 'evidence-observe', 'cancel-in-progress': false },
  jobs: {
    observe: {
      name: 'observe',
      // Merge-group runs of this repository only; a manual replay only from main. No pull
      // request event, fork or branch workflow can reach the observer or its credentials.
      if: "${{ (github.event_name == 'workflow_run' && github.event.workflow_run.event == 'merge_group' && github.event.workflow_run.conclusion != 'cancelled' && github.event.workflow_run.head_repository.full_name == github.repository) || (github.event_name == 'workflow_dispatch' && github.ref == 'refs/heads/main') }}",
      'runs-on': [evidenceGroup, evidenceRunner],
      'timeout-minutes': 30,
      defaults: { run: { shell: 'bash' } },
      steps: [
        // The default branch's checkout supplies this script and the accepted key list.
        { uses: 'actions/checkout@v4', with: { ref: 'main', 'persist-credentials': false } },
        {
          name: 'Record the finished run as evidence',
          env: {
            GH_TOKEN: '${{ github.token }}',
            EVIDENCE_PUSH_TOKEN: '${{ github.token }}',
            OBSERVED_RUN: '${{ github.event.workflow_run.id || inputs.run_id }}',
            OBSERVED_ATTEMPT: '${{ github.event.workflow_run.run_attempt || inputs.attempt }}',
          },
          run: `python3 scripts/ci-evidence-observe --repository "$GITHUB_REPOSITORY" --run "$OBSERVED_RUN" --attempt "$OBSERVED_ATTEMPT" \\
  --keys-from HEAD --scratch "$HOME/.cache/evidence-observe/scratch" --remote "https://github.com/$GITHUB_REPOSITORY.git"`,
        },
      ],
    },
  },
}, { observe: 'Reads GitHub metadata and Git objects only; builds nothing.' }))
