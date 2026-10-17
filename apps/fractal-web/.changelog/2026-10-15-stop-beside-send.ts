import type { ChangelogEntrySource } from '../src/changelog/contract.ts'

export default {
  title: 'Stop a run from the composer',
  summary: 'While an agent works, a Stop button sits beside Send, and Esc in the composer stops the run too. Stop reads Stopping… until the run ends; if st refuses, the reason shows beside it. Interrupt in the Agent menu no longer asks to confirm.',
  kind: 'feature',
} satisfies ChangelogEntrySource
