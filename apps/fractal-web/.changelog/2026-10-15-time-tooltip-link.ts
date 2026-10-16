import type { ChangelogEntrySource } from '../src/changelog/contract.ts'

export default {
  title: 'Timestamp tooltips linked to their time',
  summary: 'While a timestamp tooltip is open, the time now points to it as its description, so assistive technology reads the same exact time once. Nothing changes visually.',
  kind: 'fix',
} satisfies ChangelogEntrySource
