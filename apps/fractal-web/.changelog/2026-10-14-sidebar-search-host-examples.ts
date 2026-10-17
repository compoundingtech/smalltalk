import type { ChangelogEntrySource } from '../src/changelog/contract.ts'

export default {
  title: 'Clearer host examples in sidebar search',
  summary: 'Sidebar search hints now use simple host names such as build-1 and build-2. The examples are easier to read with a screen reader; search behavior is unchanged.',
  kind: 'fix',
} satisfies ChangelogEntrySource
