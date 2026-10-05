import { auditCaches } from './cache-audit.ts'
import { defaultActionlintConfig, githubWorkflow } from '../../repos/effect-utils/genie/external.ts'

// Preserve the public-content guard on all PRs and main pushes.
export default githubWorkflow(auditCaches({
  actionlint: defaultActionlintConfig,
  "name": "Public repository check",
  "on": {
    "pull_request": null,
    "push": {
      "branches": [
        "main"
      ]
    }
  },
  "jobs": {
    "public-repo": {
      "runs-on": "namespace-profile-linux-x86-64",
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
        }
      ]
    }
  }
}, {"public-repo": "Runs standard-library source checks without downloads or compilation."}))
