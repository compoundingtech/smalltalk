import type { ChangelogEntrySource } from '../src/changelog/contract.ts'

export default {
  title: 'Control an agent from its conversation',
  summary: 'Restart, suspend, retire, start, resume or interrupt an agent from the Agent menu in its conversation header. Each action reports its result beside the menu, and an action st refuses says why.',
  kind: 'feature',
  publicPrUrl: 'https://github.com/compoundingtech/smalltalk/pull/2778',
} satisfies ChangelogEntrySource
