import type { ChangelogEntrySource } from '../src/changelog/contract.ts'

export default {
  title: 'Agents stay visible across daemon updates',
  summary: 'A newer daemon field no longer hides an agent. If an agent cannot be read, the roster explains why; if an entire update cannot be read, your last good roster stays visible and is marked stale.',
  kind: 'fix',
} satisfies ChangelogEntrySource
