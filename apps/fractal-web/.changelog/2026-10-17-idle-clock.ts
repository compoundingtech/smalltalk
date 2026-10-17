import type { ChangelogEntrySource } from '../src/changelog/contract.ts'

export default {
  title: 'Open conversations stay responsive while idle',
  summary: 'Elapsed-time updates no longer rebuild the workspace, unchanged sidebar rows, or conversation runtime every second.',
  kind: 'fix',
} satisfies ChangelogEntrySource
