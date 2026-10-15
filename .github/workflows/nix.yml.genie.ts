import { readyPRJobs } from './ready-pr.ts'
import { defaultActionlintConfig, githubWorkflow, installNixStep } from '../../repos/effect-utils/genie/external.ts'
import { readOnlyBinaryCaches } from './workspace-ci.ts'

// ci1 retains its Nix store and per-runner GC roots. No Actions cache or FlakeHub service
// participates in this build. Fork pull requests remain covered by the public Linux gate.
export default githubWorkflow(readyPRJobs({
  actionlint: {
    ...defaultActionlintConfig,
    selfHostedRunnerLabels: [...(defaultActionlintConfig.selfHostedRunnerLabels ?? []), 'ci1'],
  },
  "name": "Nix",
  "on": {
    "schedule": [{ "cron": "17 1 * * *" }],
    // Optional Nix verification stays off PRs while the release owner repairs it.
    "workflow_dispatch": {}
  },
  "permissions": { "contents": "read" },
  "concurrency": {
    "group": "nix-${{ github.ref }}",
    "cancel-in-progress": "${{ github.event_name == 'pull_request' }}"
  },
  "jobs": {
    "nix-release-x86_64-linux": {
      // Nathan/CoS 2026-10-15: optional Nix must not occupy merge-serving workers.
      // Persistent-store verification cannot be relabelled to an ephemeral image.
      // Restore only with a reviewed, qualified dedicated persistent-store pool.
      "if": false,
      "runs-on": "ci1",
      "timeout-minutes": 180,
      "steps": [
        {
          "uses": "actions/checkout@v4",
          "with": { "persist-credentials": false }
        },
        installNixStep({ binaryCaches: readOnlyBinaryCaches }),
        {
          "name": "Test persistent-store release verification",
          "run": "python3 scripts/ci-nix-release-test"
        },
        {
          "name": "Build the package and every native check",
          "run": "scripts/ci-nix-release build"
        },
        {
          "name": "Prove the warm cache without downloads or builds",
          "run": "scripts/ci-nix-release verify-cache"
        },
        {
          "uses": "actions/upload-artifact@v4",
          "with": {
            "name": "nix-release-proof",
            "path": "${{ runner.temp }}/nix-release-build.json",
            "if-no-files-found": "error",
            "retention-days": 7
          }
        }
      ]
    }
  }
}))
