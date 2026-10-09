import { defineConfig } from 'vitest/config'

export default defineConfig({
  test: {
    setupFiles: ['src/testEnvironment.ts'],
    include: ['server/**/*.integration.test.ts', 'server/**/*.unit.test.ts', 'src/**/*.unit.test.ts', 'src/**/*.integration.test.ts'],
    // Node's own test runner owns these files; vitest finds no suite in them.
    exclude: ['**/node_modules/**', 'src/extensions/registry.unit.test.ts'],
    maxWorkers: 1,
    testTimeout: 10000,
  },
})
