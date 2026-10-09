import { auditCaches } from './cache-audit.ts'
import { defaultActionlintConfig, githubWorkflow } from '../../repos/effect-utils/genie/external.ts'

export default githubWorkflow(auditCaches({
  name: 'Namespace usage',
  on: { schedule: [{ cron: '5 3 * * *' }], workflow_dispatch: {} },
  permissions: { contents: 'read', actions: 'read' },
  actionlint: defaultActionlintConfig,
  jobs: {
    usage: {
      name: 'Previous UTC day execution minutes',
      'runs-on': 'ubuntu-latest',
      'timeout-minutes': 120,
      steps: [
        { uses: 'actions/checkout@v4', with: { 'persist-credentials': false } },
        { name: 'Report Namespace usage', env: { GH_TOKEN: '${{ secrets.CI1_RUNNERS_READ_TOKEN || github.token }}', USAGE_TOKEN_SOURCE: "${{ secrets.CI1_RUNNERS_READ_TOKEN && 'runner-read-token' || 'github-token' }}" }, run: 'python3 scripts/ci-namespace-usage --output namespace-usage.json' },
        { name: 'Retain daily usage evidence', if: 'always()', uses: 'actions/upload-artifact@v4', with: {
          name: 'namespace-usage-${{ github.run_id }}', path: 'namespace-usage.json', 'if-no-files-found': 'error', 'retention-days': 30,
        } },
      ],
    },
  },
}, { usage: 'Queries GitHub job metadata without builds or caches.' }))
