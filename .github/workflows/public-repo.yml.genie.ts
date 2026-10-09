import { linuxActionlintConfig, linuxRunner } from './workspace-ci.ts'
import { auditCaches } from './cache-audit.ts'
import { githubWorkflow } from '../../repos/effect-utils/genie/external.ts'

// Preserve the public-content guard on all PRs and main pushes.
export default githubWorkflow(auditCaches({
  actionlint: linuxActionlintConfig,
  "name": "Public repository check",
  concurrency: {
    group: "public-repo-${{ github.event.pull_request.number || (github.event_name == 'push' && github.ref == 'refs/heads/main' && 'main') || github.run_id }}",
    'cancel-in-progress': "${{ github.event_name == 'pull_request' || (github.event_name == 'push' && github.ref == 'refs/heads/main') }}",
  },
  "on": {
    "pull_request": null,
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
    "public-repo": {
      "runs-on": linuxRunner,
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
}, {"public-repo": "Runs standard-library guards and queries published release metadata; builds nothing."}))
