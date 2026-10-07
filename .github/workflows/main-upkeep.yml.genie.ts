import { buildSnapshotPrepare, buildSnapshotRestore, buildSnapshotSave } from './build-snapshot.ts'
import { auditCaches } from './cache-audit.ts'
import { defaultActionlintConfig, githubWorkflow, nixDevelopStep } from '../../repos/effect-utils/genie/external.ts'
import {
  buildEnv,
  cargoCacheStep,
  commonSetupSteps,
  linuxRunner,
  linuxStageJob,
  linuxStageRunner,
  nixCacheStep,
  perfStoresCache,
  workspacePreparationSteps,
} from './workspace-ci.ts'

const missing = "steps.cargo-probe.outputs.cache-hit != 'true' || steps.nix-probe.outputs.cache-hit != 'true'"
const whenMissing = (steps: readonly unknown[], conditionMissing = missing) => steps.map((value) => {
  const step = value as Record<string, unknown>
  const condition = String(step.if ?? 'success()').replace(/\$\{\{|\}\}/g, '')
  return { ...step, if: `success() && (${conditionMissing}) && (${condition})` }
})

// Cache scope includes the ref: merge-group saves cannot replace main's cache saves. Probe main's
// exact entries before provisioning Nix or restoring gigabytes; fill only missing entries.
const warmJob = (stage: string, setup: readonly unknown[], nixOnly = false) => ({
  name: `warm-${stage}`,
  'runs-on': stage === 'genie' ? 'ubuntu-latest' : linuxStageRunner,
  'timeout-minutes': 120,
  defaults: { run: { shell: 'bash' } },
  env: { ...buildEnv, CI_CACHE_DEV_SHELL: stage === 'genie' ? 'genie' : 'default' },
  steps: [
    { uses: 'actions/checkout@v4', with: { 'persist-credentials': false } },
    ...(nixOnly ? [nixCacheStep] : [cargoCacheStep, nixCacheStep]).map((step) => ({
      ...step,
      if: 'success()',
      id: step.id === 'cargo-cache' ? 'cargo-probe' : 'nix-probe',
      uses: 'actions/cache/restore@v4',
      with: { ...step.with, 'restore-keys': '', 'lookup-only': true },
    })),
    {
      name: 'Report cache coverage',
      env: { CARGO_HIT: nixOnly ? 'not used' : '${{ steps.cargo-probe.outputs.cache-hit }}', NIX_HIT: '${{ steps.nix-probe.outputs.cache-hit }}' },
      run: `printf 'Main cache coverage: Cargo=%s, Nix=%s\\n' "$CARGO_HIT" "$NIX_HIT" >> "$GITHUB_STEP_SUMMARY"
if ${nixOnly ? '[ \"$NIX_HIT\" != true ]' : '[ \"$CARGO_HIT\" != true ] || [ \"$NIX_HIT\" != true ]'}; then
  echo "::warning::P0: missing main-scope cache; filling it for Namespace overflow and pull requests"
fi`,
    },
    ...whenMissing([
      ...(stage === 'genie' ? setup.filter((step) => step !== buildSnapshotRestore && step !== buildSnapshotPrepare) : setup),
      nixDevelopStep({ name: 'Build missing cache contents', flake: stage === 'genie' ? '.#genie' : '.', command: ['bash', 'scripts/ci-cache-warm', stage] }),
      { name: 'Save Nix outputs', run: 'bash scripts/ci-nix-cache save' },
      // PR/merge builds restore these entries and retain exact-source artifacts. Only
      // protected main fills the shared dependency quota, including after eviction.
      ...(nixOnly ? [nixCacheStep] : [cargoCacheStep, nixCacheStep]).map((step) => ({
        name: `Save the missing main ${step.id}`,
        if: `github.ref == 'refs/heads/main' && steps.${step.id === 'cargo-cache' ? 'cargo-probe' : 'nix-probe'}.outputs.cache-hit != 'true'`,
        uses: 'actions/cache/save@v4',
        with: { path: step.with.path, key: step.with.key },
      })),
      ...(stage === 'genie' ? [] : buildSnapshotSave),
    ], nixOnly ? "steps.nix-probe.outputs.cache-hit != 'true'" : missing),
  ],
})

export default githubWorkflow(auditCaches({
  name: 'Main upkeep',
  on: { push: { branches: ['main'] }, workflow_dispatch: {} },
  permissions: { contents: 'read', actions: 'read' },
  // Fill caches for current main; keep pinned manual runs independent.
  concurrency: {
    group: "main-upkeep-${{ github.event_name == 'push' && github.ref == 'refs/heads/main' && 'main' || github.run_id }}",
    'cancel-in-progress': "${{ github.event_name == 'push' && github.ref == 'refs/heads/main' }}",
  },
  actionlint: {
    ...defaultActionlintConfig,
    selfHostedRunnerLabels: [...(defaultActionlintConfig.selfHostedRunnerLabels ?? []), ...linuxRunner, ...linuxStageRunner],
  },
  jobs: {
    'main-checks': {
      name: 'main-checks',
      'runs-on': 'ubuntu-latest',
      'timeout-minutes': 5,
      permissions: { contents: 'read', actions: 'read', checks: 'read' },
      steps: [
        { uses: 'actions/checkout@v4', with: { 'persist-credentials': false } },
        { name: 'Confirm main retains the queue checks', env: { GH_TOKEN: '${{ github.token }}' }, run: 'python3 scripts/check-main-ci' },
      ],
    },
    'cache-maintenance': {
      name: 'cache-maintenance',
      needs: ['main-checks'],
      'runs-on': 'ubuntu-latest',
      'timeout-minutes': 5,
      permissions: { contents: 'read', actions: 'write', 'pull-requests': 'read' },
      steps: [
        { uses: 'actions/checkout@v4', with: { 'persist-credentials': false } },
        { name: 'Prune closed-PR and redundant Zig caches', env: { GH_TOKEN: '${{ github.token }}' }, run: 'python3 scripts/ci-cache-prune' },
      ],
    },
    // Keep these IDs: commonSetupSteps keys the existing caches with github.job.
    'linux-tests': warmJob('tests', workspacePreparationSteps),
    'linux-clippy': warmJob('clippy', commonSetupSteps),
    'linux-fleet-compat': warmJob('fleet-compat', commonSetupSteps),
    'genie-freshness': warmJob('genie', commonSetupSteps.filter((step) => !('id' in step && step.id === 'cargo-cache')), true),
    'isolation-vm': warmJob('isolation-vm', commonSetupSteps),
    'typescript-cache': {
      name: 'warm-typescript',
      'runs-on': 'ubuntu-latest',
      'timeout-minutes': 10,
      steps: [
        { uses: 'actions/checkout@v4', with: { 'persist-credentials': false } },
        { uses: 'actions/setup-node@v4', with: { 'node-version': '24.18.0' } },
        {
          name: 'Fingerprint locked dependencies', id: 'lockfiles',
          run: `lockfiles_hash=$(sha256sum apps/ios/package-lock.json clients/typescript/st3-client/package-lock.json clients/typescript/st3-views/package-lock.json | sha256sum | cut -d ' ' -f1)
printf 'hash=%s\\n' "$lockfiles_hash" >> "$GITHUB_OUTPUT"`,
        },
        {
          name: 'Cache locked TypeScript dependencies', id: 'typescript-cache', uses: 'actions/cache@v5',
          with: {
            path: 'apps/ios/node_modules\nclients/typescript/st3-client/node_modules\nclients/typescript/st3-views/node_modules',
            key: 'typescript-client-${{ runner.os }}-node24.18.0-${{ steps.lockfiles.outputs.hash }}',
          },
        },
        {
          name: 'Fill missing dependencies', if: "steps.typescript-cache.outputs.cache-hit != 'true'",
          run: 'npm ci --prefix clients/typescript/st3-client --ignore-scripts --no-audit --no-fund\nnpm ci --prefix clients/typescript/st3-views --ignore-scripts --no-audit --no-fund\nnpm ci --prefix apps/ios --ignore-scripts --no-audit --no-fund',
        },
      ],
    },
    // This check never ran in merge_group; keep its main measurements and generated-store cache.
    'perf-cost': linuxStageJob({
      name: 'perf-cost', stage: 'cost', setup: commonSetupSteps,
      description: 'Run the cost check', command: ['bash', 'scripts/ci-perf', 'cost'],
      env: { CARGO_PROFILE_DEV_OPT_LEVEL: '1' }, extraLogs: '${{ runner.temp }}/perf/',
      before: [perfStoresCache('cost')],
    }),
  },
}, {"main-checks": "Verifies exact queue checks through the API and builds nothing.", "cache-maintenance": "Cache maintenance uses live API data and builds nothing."}))
