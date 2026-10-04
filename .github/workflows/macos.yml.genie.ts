import { buildSnapshotSave } from './build-snapshot.ts'
import { auditCaches } from './cache-audit.ts'
import {
  defaultActionlintConfig,
  githubWorkflow,
  nixDevelopStep,
} from '../../repos/effect-utils/genie/external.ts'
import { buildEnv, macosRunner, workspacePreparationSteps } from './workspace-ci.ts'

// Optional on PRs and never required. A separate workflow lets label changes start macOS without
// restarting the required Linux gate.
export default githubWorkflow(auditCaches({
  name: 'macOS CI',
  on: {
    pull_request: { types: ['opened', 'synchronize', 'reopened', 'labeled'] },
    push: { branches: ['main'] },
  },
  permissions: { contents: 'read', actions: 'read' },
  concurrency: {
    // Main pushes run independently, including while earlier runs are still pending.
    group: 'macos-${{ github.event.pull_request.number || github.run_id }}',
    'cancel-in-progress': '${{ github.event_name == \'pull_request\' }}',
  },
  actionlint: defaultActionlintConfig,
  jobs: {
    'macos-ci': {
      name: 'macos-ci',
      if: "github.event_name == 'push' || contains(github.event.pull_request.labels.*.name, 'macos-ci')",
      'runs-on': macosRunner,
      'timeout-minutes': 120,
      defaults: { run: { shell: 'bash' } },
      env: buildEnv,
      steps: [
        ...workspacePreparationSteps,
        {
          ...nixDevelopStep({ name: 'Run macOS workspace tests (25 minute limit)', command: ['bash', 'scripts/ci-nextest', 'run'] }),
          'timeout-minutes': 25,
        },
        nixDevelopStep({ name: 'Cargo clippy', command: ['cargo', 'clippy', '--workspace', '--all-targets', '--locked'] }),
        { name: 'Save Nix outputs', if: "success() && env.CI_LOCAL_CACHES != '1'", run: 'bash scripts/ci-nix-cache save' },
        ...buildSnapshotSave,
      ],
    },
  },
}, {}))
