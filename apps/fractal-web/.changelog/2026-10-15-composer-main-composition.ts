import type { ChangelogEntrySource } from '../src/changelog/contract.ts'
export default {
  title: 'Composer uses the workspace connection controls',
  summary: 'Offline and revoked drafts stay editable without duplicate recovery banners in the composer. Reconnect remains in the existing workspace footer, and blocked sends keep their draft intact.',
  kind: 'fix',
} satisfies ChangelogEntrySource
