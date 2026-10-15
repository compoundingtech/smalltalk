import type { ChangelogEntrySource } from '../src/changelog/contract.ts'

export default {
  title: 'No more raw tool records in the transcript',
  summary: 'An omp agent\'s tool start no longer appears as a block of JSON under its tool rows. The tool row already shows the call, so the record now stays out of the conversation and remains available when system events are shown.',
  kind: 'fix',
} satisfies ChangelogEntrySource
