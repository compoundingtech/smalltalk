import type { ChangelogEntrySource } from '../src/changelog/contract.ts'

export default {
  title: 'Restarting seats say so',
  summary: 'A seat that is being replaced after a declaration change now reads restarting in the sidebar, shown as pending, instead of an unknown state.',
  kind: 'improvement',
} satisfies ChangelogEntrySource
