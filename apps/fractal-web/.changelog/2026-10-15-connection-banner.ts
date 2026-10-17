import type { ChangelogEntrySource } from '../src/changelog/contract.ts'

export default {
  title: 'One connection banner',
  summary: 'When the connection drops, one band under the thread header says so and offers Reconnect, instead of a line in the sidebar footer. The roster and Missions states, and the loading rows for Changes, documents and the terminal, now share one short, quiet row that does not repeat the connection.',
  kind: 'improvement',
} satisfies ChangelogEntrySource
