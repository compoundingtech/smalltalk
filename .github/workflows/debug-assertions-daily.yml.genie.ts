import { githubWorkflow } from '../../repos/effect-utils/genie/external.ts'
import { auditCaches } from './cache-audit.ts'
import { linuxActionlintConfig, linuxStageJob, workspacePreparationSteps } from './workspace-ci.ts'

// The dev and test profiles build without debug assertions (Cargo.toml). Once a day this runs the
// whole required test set once with them on, so a debug_assert!, std's unsafe-precondition checks
// and the st2 tests that drive the ST2_TEST_* seams (ignored without assertions) still run. A
// failure opens, or comments on, one open issue titled `Daily debug-assertions run failed`.
// Namespace only, one run a day, never on pull requests, and nothing here is a required check.
const title = 'Daily debug-assertions run failed'

export default githubWorkflow(auditCaches({
  actionlint: linuxActionlintConfig,
  name: 'Debug assertions daily',
  on: {
    schedule: [{ cron: '41 3 * * *' }],
    workflow_dispatch: {},
  },
  permissions: { contents: 'read' },
  concurrency: { group: 'debug-assertions-daily', 'cancel-in-progress': false },
  jobs: {
    'debug-assertions-tests': {
      ...linuxStageJob({
        name: 'debug-assertions-tests',
        stage: 'tests',
        setup: workspacePreparationSteps,
        description: 'Run nextest with debug assertions on',
        env: { CARGO_PROFILE_DEV_DEBUG_ASSERTIONS: 'true', CARGO_PROFILE_TEST_DEBUG_ASSERTIONS: 'true' },
        after: [{
          name: 'Report a failure in one open issue',
          if: 'failure()',
          env: { GH_TOKEN: '${{ github.token }}', GH_REPO: '${{ github.repository }}', TITLE: title },
          run: `body="Run \${GITHUB_SERVER_URL}/\${GITHUB_REPOSITORY}/actions/runs/\${GITHUB_RUN_ID} at \${GITHUB_SHA} failed with CARGO_PROFILE_DEV_DEBUG_ASSERTIONS=true and CARGO_PROFILE_TEST_DEBUG_ASSERTIONS=true. The tests that need the assertions (the ST2_TEST_* seams) only run here; see the job summary and the debug-assertions-tests-logs artifact."
existing=$(gh issue list --state open --search "\\"$TITLE\\" in:title" --json number --jq '.[0].number // empty')
if [ -n "$existing" ]; then gh issue comment "$existing" --body "$body"; else gh issue create --title "$TITLE" --body "$body"; fi`,
        }],
      }),
      permissions: { contents: 'read', issues: 'write', actions: 'read' },
    },
  },
}, {}))
