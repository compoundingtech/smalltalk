import type { ChangelogEntrySource } from '../src/changelog/contract.ts'

export default {
  title: 'Keep internal tool records out of transcript prose',
  summary: 'Internal harness records no longer appear as raw JSON messages in the transcript. Visible system messages, including messages containing JSON, remain readable.',
  kind: 'fix',
} satisfies ChangelogEntrySource
