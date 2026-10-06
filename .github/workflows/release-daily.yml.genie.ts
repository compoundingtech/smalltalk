import { linuxActionlintConfig, linuxRunner } from './workspace-ci.ts'
import { auditCaches } from './cache-audit.ts'
import { githubWorkflow } from '../../repos/effect-utils/genie/external.ts'

// Publishes once a day, only when main changed since the last release. It builds nothing: it
// takes the archives that the newest successful main run of release-smalltalk.yml uploaded.
// Tags made with this workflow's token do not start the tag workflow, so there is no second build.
export default githubWorkflow(auditCaches({
  actionlint: linuxActionlintConfig,
  name: 'Smalltalk daily release',
  on: {
    schedule: [{ cron: '17 5 * * *' }],
    workflow_dispatch: {
      inputs: {
        tag: {
          description: 'Release tag; default is the next patch after the highest v tag',
          required: false,
          type: 'string',
        },
      },
    },
  },
  permissions: { contents: 'read' },
  concurrency: { group: 'smalltalk-daily-release', 'cancel-in-progress': false },
  jobs: {
    release: {
      name: 'daily-release',
      'runs-on': linuxRunner,
      'timeout-minutes': 20,
      permissions: { contents: 'write', actions: 'read' },
      env: {
        GH_TOKEN: '${{ github.token }}',
        GH_REPO: '${{ github.repository }}',
        TAG: '${{ inputs.tag }}',
      },
      steps: [
        { uses: 'actions/checkout@v4', with: { 'fetch-depth': 0 } },
        {
          name: 'Publish the verified archives of main when it changed',
          run: 'scripts/release-smalltalk-daily ${TAG:+--tag "$TAG"}',
        },
      ],
    },
  },
}, {"release": "Reuses verified main release artifacts; builds nothing."}))
