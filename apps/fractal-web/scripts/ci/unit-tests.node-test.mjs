import { test } from 'node:test'
import assert from 'node:assert/strict'
import { selectTests } from './unit-tests.mjs'

const modified = path => ['M', path]

test('app, kit and client modules select their importers through the import graph', () => {
  assert.deepEqual(selectTests([
    modified('apps/fractal-web/src/shell/AgentFolders.tsx'),
    ['A', 'packages/fractal-ui/src/assistant-ui/shell.ts'],
    modified('clients/typescript/st3-client/Schema.generated.ts'),
    modified('apps/fractal-web/src/shell/AgentFolders.css'),
  ]), { mode: 'related', files: [
    'apps/fractal-web/src/shell/AgentFolders.tsx', 'packages/fractal-ui/src/assistant-ui/shell.ts',
    'clients/typescript/st3-client/Schema.generated.ts', 'apps/fractal-web/src/shell/AgentFolders.css',
  ] })
})

test('files outside the app, kit and client trees cannot affect the app tests', () => {
  assert.deepEqual(selectTests([modified('docs/ci.md'), modified('crates/st3/src/lib.rs'), modified('release-notes/x.json')]), { mode: 'related', files: [] })
  assert.deepEqual(selectTests([]), { mode: 'related', files: [] })
})

test('anything the import graph cannot place runs every test', () => {
  for (const [entry, reason] of [
    [modified('package.json'), /package manifest/],
    [modified('apps/fractal-web/package.json'), /package manifest/],
    [modified('pnpm-lock.yaml'), /lockfile/],
    [modified('flake.lock'), /lockfile or toolchain/],
    [modified('apps/fractal-web/tsconfig.json'), /TypeScript configuration/],
    [modified('apps/fractal-web/vitest.config.ts'), /Vite or Vitest/],
    [modified('apps/fractal-web/vite.config.ts'), /Vite or Vitest/],
    [modified('apps/fractal-web/src/testEnvironment.ts'), /test environment/],
    [modified('apps/fractal-web/scripts/changelog-registry.mjs'), /app scripts/],
    [modified('apps/fractal-web/scripts/ci/unit-tests.mjs'), /app scripts/],
    [modified('scripts/ci-fractal-web'), /CI input/],
    [modified('.github/workflows/fleet.yml'), /CI input/],
    [['D', 'apps/fractal-web/src/web/gone.ts'], /deleted/],
    [['R100', 'apps/fractal-web/src/web/a.ts'], /changed in a way/],
    [modified('apps/fractal-web/src/web/fixture.json'), /not a module/],
    [modified('packages/fractal-ui/src/fonts/Inter.woff2'), /not a module/],
  ]) {
    const selection = selectTests([modified('apps/fractal-web/src/shell/AgentFolders.tsx'), entry])
    assert.equal(selection.mode, 'all', entry.join(' '))
    assert.match(selection.reason, reason, entry.join(' '))
  }
})
