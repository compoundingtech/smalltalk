import { defaultActionlintConfig, githubWorkflow, installNixStep } from '../../repos/effect-utils/genie/external.ts'
import { readOnlyBinaryCaches } from './workspace-ci.ts'

// ci1 retains its Nix store and per-runner GC roots. No Actions cache or FlakeHub service
// participates in this build. Fork pull requests remain covered by the public Linux gate.
export default githubWorkflow({
  actionlint: {
    ...defaultActionlintConfig,
    selfHostedRunnerLabels: [...(defaultActionlintConfig.selfHostedRunnerLabels ?? []), 'ci1'],
  },
  "name": "Nix",
  "on": {
    "push": {
      "tags": [
        "**"
      ]
    },
    "schedule": [{ "cron": "17 1 * * *" }],
    "workflow_dispatch": {},
    "pull_request": {
      "paths": [".github/workflows/nix.yml*", ".github/messaging-compat-baseline.json", "scripts/ci-nix-release*", "flake.nix", "flake.lock", "nix/**"]
    }
  },
  "permissions": { "contents": "read" },
  "concurrency": {
    "group": "nix-${{ github.ref }}",
    "cancel-in-progress": false
  },
  "jobs": {
    "nix-release-x86_64-linux": {
      "if": "github.event_name != 'pull_request' || github.event.pull_request.head.repo.full_name == github.repository",
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
})
