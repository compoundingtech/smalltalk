import type { ChangelogEntrySource } from '../src/changelog/contract.ts'

export default {
  title: 'Show the themed shell immediately',
  summary: 'The app stylesheet now loads through the build pipeline, so the themed shell paints immediately and styles apply under the strict content security policy.',
  kind: 'fix',
  publicPrUrl: 'https://github.com/compoundingtech/smalltalk/pull/2470',
} satisfies ChangelogEntrySource
