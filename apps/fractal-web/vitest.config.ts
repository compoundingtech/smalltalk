import { defineConfig } from 'vitest/config'

// Only the app's Vite build supplies `virtual:build-identity`. jsdom suites transform modules for the
// web, where import analysis must resolve it before `vi.mock` replaces it; suites mock the values.
const buildIdentity = {
  name: 'test:build-identity',
  enforce: 'pre' as const,
  resolveId: (id: string) => id === 'virtual:build-identity' ? '\0test-build-identity' : undefined,
  load: (id: string) => id === '\0test-build-identity' ? `export const buildIdentity = ${JSON.stringify({ displayVersion: 'test', machineVersion: 'test', dirty: false })};\nexport const deploymentId = undefined;\n` : undefined,
}

export default defineConfig({
  plugins: [buildIdentity],
  test: {
    globalSetup: ['scripts/changelog-registry-setup.mjs'],
    setupFiles: ['src/testEnvironment.ts'],
    include: ['server/**/*.integration.test.ts', 'server/**/*.unit.test.ts', 'src/**/*.unit.test.ts', 'src/**/*.unit.test.tsx', 'src/**/*.integration.test.ts', 'src/**/*.integration.test.tsx'],
    // Node's own test runner owns these files; vitest finds no suite in them.
    exclude: ['**/node_modules/**', 'src/extensions/registry.unit.test.ts'],
    maxWorkers: 1,
    testTimeout: 10000,
  },
})
