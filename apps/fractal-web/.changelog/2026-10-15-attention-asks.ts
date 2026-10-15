import type { ChangelogEntrySource } from '../src/changelog/contract.ts'

export default {
  title: 'Needs me counts answerable asks',
  summary: 'Agent attention badges and the Needs me sidebar filter now include only open native requests, prompts and custom asks with supported advertised answers. Updates, messages, faults and terminal-only prompts no longer inflate the count.',
  kind: 'fix',
} satisfies ChangelogEntrySource
