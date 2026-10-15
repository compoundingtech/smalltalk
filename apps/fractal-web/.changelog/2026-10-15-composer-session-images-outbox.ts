import type { ChangelogEntrySource } from '../src/changelog/contract.ts'
export default {
  title: 'Messages keep their conversation context',
  summary: 'Composer sends carry the active native session and image references, while failed uploads retain their files without sending. Offline, missing permission and revoked credentials have distinct draft-preserving states and never resend automatically. Retry preserves the original full payload and key.',
  kind: 'fix',
} satisfies ChangelogEntrySource
