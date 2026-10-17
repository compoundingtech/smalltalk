import { readyPRJobs } from './ready-pr.ts'
import { githubWorkflow, defaultActionlintConfig } from '../../repos/effect-utils/genie/external.ts'

// SOURCE ONLY, UNRUN. Root must own-read this exact binding before Ready.
// Same Namespace PR priority/profile as workspace-ci.ts; no shared CI host.
// No dispatch/rerun/push/schedule trigger. Job-local code cannot cancel queued work.
const workflow = {
  "name": "Receive replay F1 disposable mechanism",
  "on": {
    "pull_request": {
      "types": [
        "ready_for_review"
      ]
    }
  },
  "permissions": {
    "contents": "read",
    "actions": "read",
    "pull-requests": "read"
  },
  "jobs": {
    "receive-replay-f1": {
      "name": "receive-replay-f1",
      "if": "github.event_name == 'pull_request' && github.event.action == 'ready_for_review' && github.event.pull_request.number == 2864 && github.event.pull_request.head.repo.full_name == github.repository && github.run_attempt == 1",
      "runs-on": [
        "namespace-profile-linux-x86-64;job.priority=2;github.run-id=${{ github.run_id }}"
      ],
      "timeout-minutes": 2,
      "steps": [
        {
          "uses": "actions/checkout@v4",
          "with": {
            "ref": "${{ github.event.pull_request.head.sha }}",
            "persist-credentials": false
          }
        },
        {
          "name": "One serial disposable F1 controller",
          "env": {
            "GH_TOKEN": "${{ github.token }}"
          },
          "run": "python3 -I scripts/receive-replay-f1/controller.py"
        },
        {
          "name": "Retain bounded terminal receipt",
          "if": "always()",
          "uses": "actions/upload-artifact@v4",
          "with": {
            "name": "receive-replay-f1-${{ github.run_id }}",
            "path": "${{ runner.temp }}/receive-replay-f1-*.json",
            "if-no-files-found": "warn",
            "retention-days": 7
          }
        }
      ]
    }
  }
}

export default githubWorkflow(readyPRJobs({ ...workflow, actionlint: { ...defaultActionlintConfig, selfHostedRunnerLabels: [...(defaultActionlintConfig.selfHostedRunnerLabels ?? []), "namespace-profile-linux-x86-64;job.priority=2;github.run-id=${{ github.run_id }}"] } }))
