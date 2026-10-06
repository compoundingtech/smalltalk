import { defineConfig } from 'vitest/config'
export default defineConfig({ test: { include: ['server/**/*.integration.test.ts', 'src/**/*.unit.test.ts', 'src/**/*.integration.test.ts'], maxWorkers: 1, testTimeout: 10000 } })
