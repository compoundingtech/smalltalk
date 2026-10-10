import { defineConfig } from 'vitest/config'

// Pins the root here; without it vitest picks up the repository root's projects.
export default defineConfig({
  test: { root: import.meta.dirname, include: ['test/**/*.test.ts', 'test/**/*.test.tsx'] },
})
