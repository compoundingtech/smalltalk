import { auditCaches } from './cache-audit.ts'
import { defaultActionlintConfig, githubWorkflow } from '../../repos/effect-utils/genie/external.ts'

// A capacity observation needs one authenticated Namespace runner, without a workspace build.
export default githubWorkflow(auditCaches({
  name: 'Namespace capacity',
  on: { workflow_dispatch: {}, schedule: [{ cron: '17 * * * *' }] },
  permissions: { contents: 'read' },
  concurrency: { group: 'namespace-capacity', 'cancel-in-progress': true },
  actionlint: defaultActionlintConfig,
  jobs: {
    'namespace-capacity': {
      name: 'namespace-capacity',
      'runs-on': ['namespace-profile-linux-x86-64;job.priority=3;github.run-id=${{ github.run_id }}'],
      'timeout-minutes': 120,
      defaults: { run: { shell: 'bash' } },
      steps: [
        {
          name: 'Record Namespace platform capacity',
          run: `nsc workspace concurrency --output json | jq '{observed_at: (now | todateiso8601), concurrency: [.concurrency[] | {platforms, limits, activeConcurrency}]}' > "$RUNNER_TEMP/namespace-capacity.json"
cat "$RUNNER_TEMP/namespace-capacity.json"
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
            'retention-days': 30,
          },
        },
      ],
    },
  },
}, { 'namespace-capacity': 'Reads Namespace concurrency without builds or caches.' }))
