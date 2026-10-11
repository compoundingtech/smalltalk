import { defineConfig } from 'vitest/config'

// The parent app deliberately includes only its own *.unit/*.integration tests. The SDK
// also owns admission.test.ts and followFreshness.test.ts; never inherit that narrower set.
export default defineConfig({
  test: {
    include: ['src/**/*.test.ts'],
    exclude: ['**/node_modules/**', 'src/**/*.perf.test.ts'],
    maxWorkers: 1,
    testTimeout: 10000,
  },
})
