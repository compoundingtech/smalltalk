import { readyPRJobs, readyPullRequestTypes } from './ready-pr.ts'
import { linuxActionlintConfig } from './workspace-ci.ts'
import { auditCaches } from './cache-audit.ts'
import { githubWorkflow } from '../../repos/effect-utils/genie/external.ts'

// Label changes refresh the changelog opt-out; drafts still allocate no runners.
export default githubWorkflow(readyPRJobs(auditCaches({
  actionlint: linuxActionlintConfig,
  "name": "Public repository check",
  concurrency: {
    group: "public-repo-${{ github.event.pull_request.number || (github.event_name == 'push' && github.ref == 'refs/heads/main' && 'main') || github.run_id }}",
    'cancel-in-progress': "${{ github.event_name == 'pull_request' || (github.event_name == 'push' && github.ref == 'refs/heads/main') }}",
  },
  "on": {
    "pull_request": { types: [...readyPullRequestTypes, 'labeled', 'unlabeled'] },
    "merge_group": null,
    "workflow_dispatch": null,
    "push": {
      "branches": [
        "main"
      ]
    }
  },
  permissions: { contents: 'read' },
  "jobs": {
    'fractal-changelog': {
      name: 'fractal-changelog',
      if: "github.event_name == 'pull_request'",
      'runs-on': 'ubuntu-latest',
      'timeout-minutes': 5,
      permissions: { contents: 'read' },
      steps: [
        // PR code executes only on a disposable hosted runner, with no secrets or write token.
        { uses: 'actions/checkout@v4', with: { 'fetch-depth': 0, 'persist-credentials': false } },
        { name: 'Test changelog requirement policy', run: 'node --test apps/fractal-web/scripts/ci/changelog-policy.unit.test.mjs' },
        {
          name: 'Require a new changelog entry for UI changes',
          env: {
            PR_MERGE_SHA: '${{ github.sha }}',
            PR_LABELS: '${{ toJSON(github.event.pull_request.labels.*.name) }}',
          },
          run: 'node apps/fractal-web/scripts/ci/changelog-requirement.mjs --base "$PR_MERGE_SHA^1" --head "$PR_MERGE_SHA" --labels "$PR_LABELS"',
        },
      ],
    },
    "public-repo": {
      "runs-on": 'ubuntu-latest',
      "steps": [
        {
          "uses": "actions/checkout@v4"
        },
        {
          "name": "Test repository guard",
          "run": "python3 scripts/check-public-repo-test"
        },
        {
          "name": "Check repository content",
          "run": "python3 scripts/check-public-repo"
        },
        {
          name: 'Test rolling compatibility baseline policy',
          run: 'python3 scripts/compat-baseline-release-test\npython3 scripts/compat-baseline-test\n',
        },
        {
          name: 'Check published stable release baseline freshness',
          env: { GH_TOKEN: '${{ github.token }}', GH_REPO: '${{ github.repository }}' },
          run: 'python3 scripts/compat-baseline-release check',
        },
      ]
    }
  }
}, {"public-repo": "Runs standard-library guards and queries published release metadata; builds nothing.", "fractal-changelog": "Checks the full Git diff and pure changelog policy without dependencies or compilation."})))
