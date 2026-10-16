import type { ChangelogEntrySource } from '../src/changelog/contract.ts'

export default {
  title: 'Answer alerts in their conversation',
  summary: 'Every alert for an agent, including asks and review gates, now shows just above the composer in its conversation. Answer it there, and it clears when st records the answer.',
  kind: 'feature',
  publicPrUrl: 'https://github.com/compoundingtech/smalltalk/pull/2748',
} satisfies ChangelogEntrySource
