import type { ChangelogEntrySource } from '../src/changelog/contract.ts'

export default {
  title: 'The inbox says when it cannot tell what needs you',
  summary: 'While your alerts are loading, could not be read, or were last seen before the connection dropped, the inbox now says so instead of reading "Nothing needs you".',
  kind: 'fix',
  publicPrUrl: 'https://github.com/compoundingtech/smalltalk/pull/3100',
} satisfies ChangelogEntrySource
