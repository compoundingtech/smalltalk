import { defineConfig } from 'vitest/config'

// Timing belongs to its own named CI lane, never the deterministic SDK unit suite.
export default defineConfig({
  test: { include: ['src/**/*.perf.test.ts'], maxWorkers: 1, testTimeout: 10000 },
})
