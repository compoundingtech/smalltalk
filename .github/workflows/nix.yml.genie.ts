import { defaultActionlintConfig, githubWorkflow, installNixStep } from '../../repos/effect-utils/genie/external.ts'
import { readOnlyBinaryCaches } from './workspace-ci.ts'

// Preserve the tag-only Nix release/check graph; fork workspace coverage is now linux-gate.
export default githubWorkflow({
  actionlint: defaultActionlintConfig,
  "name": "Nix",
  "on": {
    "push": {
      "tags": [
        "**"
      ]
    }
  },
  "concurrency": {
    "group": "nix-${{ github.workflow }}-${{ github.ref }}",
    "cancel-in-progress": "${{ github.event_name == 'pull_request' }}"
  },
  "jobs": {
    "nix-release-x86_64-linux": {
      "if": "${{ startsWith(github.ref, 'refs/tags/') }}",
      "runs-on": "namespace-profile-linux-x86-64",
      "timeout-minutes": 180,
      "permissions": {
        "contents": "read",
        "id-token": "write"
      },
      "steps": [
        {
          "uses": "actions/checkout@v4"
        },
        installNixStep({ binaryCaches: readOnlyBinaryCaches }),
        {
          "uses": "DeterminateSystems/magic-nix-cache-action@main"
        },
        {
          "run": "nix build --no-link --print-build-logs .#packages.x86_64-linux.default .#checks.x86_64-linux.st2 .#checks.x86_64-linux.st3 .#checks.x86_64-linux.debug-assertions .#checks.x86_64-linux.help .#checks.x86_64-linux.completions .#checks.x86_64-linux.hooks-replacement .#checks.x86_64-linux.pty-fleet-contract .#checks.x86_64-linux.st3-help .#checks.x86_64-linux.install-layout .#checks.x86_64-linux.st2-install-layout .#checks.x86_64-linux.provider-components .#checks.x86_64-linux.wasip2-resource-providers .#checks.x86_64-linux.release-integration .#checks.x86_64-linux.pi-extension-types .#checks.x86_64-linux.wasm-resolver-feature .#checks.x86_64-linux.wasm-resolver-artifact"
        }
      ]
    }
  }
})
