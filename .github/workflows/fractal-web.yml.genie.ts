import { linuxActionlintConfig, linuxRunner } from './workspace-ci.ts'
import { auditCaches } from './cache-audit.ts'
import { githubWorkflow } from '../../repos/effect-utils/genie/external.ts'

// Inputs whose change can alter the fixtures gate's result. Rust-only changes match none of them.
const relevant = [
  'apps/fractal-web/',
  'clients/typescript/st3-client/',
  '.github/workflows/fractal-web.yml',
  '.github/workflows/fractal-web.yml.genie.ts',
]

// The `fractal-web` job always reports on pull_request and merge_group, so it can become a
// required check. Execution runs only when relevant inputs changed; detection failure fails closed.
export default githubWorkflow(auditCaches({
  actionlint: linuxActionlintConfig,
  name: 'fractal-web',
  concurrency: {
    group: "fractal-web-${{ github.event.pull_request.number || github.event.merge_group.head_ref || github.run_id }}",
    'cancel-in-progress': "${{ github.event_name == 'pull_request' }}",
  },
  on: {
    pull_request: null,
    merge_group: {},
  },
  permissions: { contents: 'read' },
  jobs: {
    'fractal-web-detect': {
      'runs-on': linuxRunner,
      'timeout-minutes': 5,
      outputs: { relevant: '${{ steps.detect.outputs.relevant }}' },
      steps: [
        { uses: 'actions/checkout@v4', with: { 'fetch-depth': 0, 'persist-credentials': false } },
        {
          name: 'Detect fractal-web inputs between the immutable base and head',
          id: 'detect',
          shell: 'bash',
          env: {
            BASE_SHA: '${{ github.event.pull_request.base.sha || github.event.merge_group.base_sha }}',
            HEAD_SHA: '${{ github.event.pull_request.head.sha || github.event.merge_group.head_sha }}',
            RELEVANT: relevant.join('\n'),
          },
          run: `set -euo pipefail
test -n "$BASE_SHA" && test -n "$HEAD_SHA"
changed=$(git diff --name-only "$BASE_SHA" "$HEAD_SHA")
hit=false
while IFS= read -r file; do
  while IFS= read -r prefix; do
    case "$file" in "$prefix"*) hit=true ;; esac
  done <<< "$RELEVANT"
done <<< "$changed"
printf 'relevant=%s\\n' "$hit" >> "$GITHUB_OUTPUT"
printf 'fractal-web inputs changed: **%s**\\n' "$hit" >> "$GITHUB_STEP_SUMMARY"`,
        },
      ],
    },
    'fractal-web-fixtures': {
      needs: ['fractal-web-detect'],
      if: "needs.fractal-web-detect.outputs.relevant == 'true'",
      'runs-on': linuxRunner,
      'timeout-minutes': 10,
      defaults: { run: { shell: 'bash' } },
      steps: [
        { uses: 'actions/checkout@v4', with: { 'persist-credentials': false } },
        { uses: 'actions/setup-node@v4', with: { 'node-version': '24.18.0' } },
        { name: 'Install the client schema runtime', run: 'npm ci --prefix clients/typescript/st3-client --ignore-scripts --no-audit --no-fund' },
        { name: 'Generate, decode, scan and check synthetic fixtures', run: 'bash apps/fractal-web/scripts/ci/fixtures-gate.sh' },
      ],
    },
    'fractal-web': {
      name: 'fractal-web',
      needs: ['fractal-web-detect', 'fractal-web-fixtures'],
      if: '${{ !cancelled() }}',
      'runs-on': linuxRunner,
      'timeout-minutes': 5,
      steps: [
        {
          name: 'Require detection, and execution when relevant',
          shell: 'bash',
          env: {
            DETECT: '${{ needs.fractal-web-detect.result }}',
            RELEVANT: '${{ needs.fractal-web-detect.outputs.relevant }}',
            FIXTURES: '${{ needs.fractal-web-fixtures.result }}',
          },
          run: `set -euo pipefail
test "$DETECT" = success || { echo "::error::fractal-web change detection: $DETECT"; exit 1; }
if [ "$RELEVANT" = true ]; then
  test "$FIXTURES" = success || { echo "::error::fractal-web fixtures gate: $FIXTURES"; exit 1; }
elif [ "$RELEVANT" != false ]; then
  echo "::error::fractal-web change detection returned no decision"; exit 1
fi`,
        },
      ],
    },
  },
}, {
  'fractal-web-detect': 'Compares the event base and head with git only.',
  'fractal-web-fixtures': 'Installs one locked package; the fixtures gate itself is source-only.',
  'fractal-web': 'Aggregates job results without builds.',
}))
