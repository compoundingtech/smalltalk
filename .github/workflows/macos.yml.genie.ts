import {
  defaultActionlintConfig,
  githubWorkflow,
  nixDevelopStep,
} from '../../repos/effect-utils/genie/external.ts'
import { buildEnv, macosRunner, workspacePreparationSteps } from './workspace-ci.ts'

// Optional and never required. A separate workflow lets label changes start macOS without
// restarting the required Linux gate.
export default githubWorkflow({
  name: 'macOS CI',
  on: {
    pull_request: { types: ['opened', 'synchronize', 'reopened', 'labeled'] },
    push: { branches: ['main'] },
  },
  permissions: { contents: 'read' },
  concurrency: {
    group: 'macos-${{ github.event.pull_request.number || github.ref }}',
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
          ...nixDevelopStep({ name: 'Run macOS workspace tests (25 minute limit)', command: ['cargo', 'nextest', 'run', '--workspace', '--locked', '--profile', 'ci'] }),
          'timeout-minutes': 25,
        },
        nixDevelopStep({ name: 'Cargo clippy', command: ['cargo', 'clippy', '--workspace', '--all-targets', '--locked'] }),
      ],
    },
  },
})
