import type { ChangelogEntrySource } from '../src/changelog/contract.ts'

export default {
  title: 'Search the live sidebar folders by name, facet or machine',
  summary: 'The live sidebar now shows agents in folders, and search matches agent names and IDs, so plain text no longer matches host names; use machine:NAME or host:NAME for that. Narrow results with repo:, state: and tool:, or show only agents that need you and hide finished ones. Press / to search and ArrowDown to move into the matching agents.',
  kind: 'feature',
} satisfies ChangelogEntrySource
