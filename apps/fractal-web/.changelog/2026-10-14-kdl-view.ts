import type { ChangelogEntrySource } from '../src/changelog/contract.ts'

export default {
  title: 'View agent declarations without leaving the conversation',
  summary:
    'Open View KDL from the conversation header or an agent menu to read and copy the agent declaration with syntax highlighting and line numbers. Agents with a current mission also offer View mission definition, shown as a labeled structured JSON fallback rather than canonical KDL.',
  kind: 'improvement',
} satisfies ChangelogEntrySource
