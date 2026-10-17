import type { ChangelogEntrySource } from '../src/changelog/contract.ts'

export default {
  title: 'Tidier sidebar, tool rows and tabs',
  summary: 'Agents in the sidebar no longer scroll up against the search field. A running tool says Running right after its name instead of at the far edge of the row. Workspace tabs read Thread, Missions and Usage in the same case.',
  kind: 'fix',
} satisfies ChangelogEntrySource
