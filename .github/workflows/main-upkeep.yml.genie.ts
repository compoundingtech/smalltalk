import { buildSnapshotSave } from './build-snapshot.ts'
import { auditCaches } from './cache-audit.ts'
import { defaultActionlintConfig, githubWorkflow, nixDevelopStep, plainFlakeSetupSteps } from '../../repos/effect-utils/genie/external.ts'
import {
  buildEnv,
  cargoCacheStep,
  commonSetupSteps,
  fractalWebStoreCache,
  linuxRunner,
  linuxStageJob,
  linuxStageRunner,
  megarepoApplyStep,
  nixCacheStep,
  perfStoresCache,
  pnpmStoreEnv,
  readOnlyBinaryCaches,
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
  'runs-on': linuxStageRunner,
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
      ...setup,
      ...(stage === 'genie' ? [megarepoApplyStep] : []),
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
      ...buildSnapshotSave,
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
      'runs-on': linuxStageRunner,
      'timeout-minutes': 10,
      env: { COREPACK_ENABLE_DOWNLOAD_PROMPT: '0' },
      steps: [
        { uses: 'actions/checkout@v4', with: { 'persist-credentials': false } },
        { uses: 'actions/setup-node@v4', with: { 'node-version': '24.18.0' } },
        {
          name: 'Fingerprint locked dependencies', id: 'lockfiles',
          run: `lockfiles_hash=$(sha256sum pnpm-lock.yaml | cut -d ' ' -f1)
printf 'hash=%s\\n' "$lockfiles_hash" >> "$GITHUB_OUTPUT"`,
        },
        {
          name: 'Cache locked TypeScript dependencies', id: 'typescript-cache', uses: 'actions/cache@v5',
          with: {
            path: pnpmStoreEnv.pnpm_config_store_dir,
            key: 'typescript-client-${{ runner.os }}-node24.18.0-pnpm12.7.0-${{ steps.lockfiles.outputs.hash }}',
          },
        },
        {
          name: 'Fill missing dependencies', if: "steps.typescript-cache.outputs.cache-hit != 'true'", env: pnpmStoreEnv,
          run: 'corepack enable\npnpm fetch --frozen-lockfile',
        },
      ],
    },
    // Fractal-web runs on GitHub-hosted capacity, whose cache is separate from Namespace's.
    'fractal-web-cache': {
      name: 'warm-fractal-web',
      'runs-on': 'ubuntu-latest',
      'timeout-minutes': 15,
      defaults: { run: { shell: 'bash' } },
      steps: [
        { uses: 'actions/checkout@v4', with: { 'persist-credentials': false } },
        { name: 'Probe the pnpm store', id: 'pnpm-store', uses: 'actions/cache/restore@v4', with: { ...fractalWebStoreCache, 'lookup-only': true } },
        ...[
          ...plainFlakeSetupSteps({ nix: { binaryCaches: readOnlyBinaryCaches } }),
          { ...nixDevelopStep({ name: 'Fetch the locked packages', flake: '.#web', command: ['pnpm', 'fetch', '--frozen-lockfile'] }), env: pnpmStoreEnv },
          { name: 'Save the pnpm store', uses: 'actions/cache/save@v4', with: fractalWebStoreCache },
        ].map((step) => ({ ...step, if: "steps.pnpm-store.outputs.cache-hit != 'true'" })),
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
