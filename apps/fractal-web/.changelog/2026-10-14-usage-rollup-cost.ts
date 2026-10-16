import type { ChangelogEntrySource } from '../src/changelog/contract.ts'

export default {
  title: 'Keep rollup costs in agent usage',
  summary: 'Agent usage includes the selected rollup cost without double counting. Cost stays unknown if a contributing price or currency is missing, or if currencies differ. The app and daemon must update together; older apps cannot read the new usage aggregation value.',
  kind: 'fix',
  publicPrUrl: 'https://github.com/compoundingtech/smalltalk/pull/1473',
} satisfies ChangelogEntrySource
