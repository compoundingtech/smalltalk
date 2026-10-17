import type { ChangelogEntrySource } from '../src/changelog/contract.ts'

export default {
  title: 'Keep keyboard focus when stopping a run',
  summary: 'Stop keeps keyboard focus while the stop is pending and ignores repeated presses. When the run ends, focus returns to Message, or to the composer if Message is unavailable.',
  kind: 'fix',
} satisfies ChangelogEntrySource
