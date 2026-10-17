import { defineConfig } from 'vitest/config'

// Pins the root here; without it vitest picks up the repository root's projects.
export default defineConfig({
  // Recording JSON is deliberately substantial; do not transform one copy per host CPU.
  test: { root: import.meta.dirname, include: ['test/**/*.test.ts', 'test/**/*.test.tsx'], maxWorkers: 2 },
})
