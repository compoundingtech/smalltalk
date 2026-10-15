import type { ChangelogEntrySource } from '../src/changelog/contract.ts'

export default {
  title: 'Type directly in agent terminals',
  summary: 'Click a terminal and type: keys, input methods and pastes go straight to the shell, and output appears as it happens. A chip shows whether you are typing, reconnecting or read-only. When typing is not allowed or the connection fails, the terminal stays visible read-only and says why.',
  kind: 'feature',
} satisfies ChangelogEntrySource
