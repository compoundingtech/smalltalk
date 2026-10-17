import type { ChangelogEntrySource } from '../src/changelog/contract.ts'

export default {
  title: 'Agent Details and Resources beside the conversation',
  summary: 'The agent inspector opens on a Details tab with identity, mission, status, session timing, context, spend, resources and traced subagents, each naming what was not reported. Resources opens as a side panel that keeps the conversation usable, starts with the agent\'s Todos and reopens instantly with its last reading. Time labels update only at their next visible change and pause while the page is hidden.',
  kind: 'feature',
} satisfies ChangelogEntrySource
