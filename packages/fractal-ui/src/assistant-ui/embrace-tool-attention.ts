/** Pure shared tool classification/attention; no assistant-ui runtime or React dependency. */
import type { ConversationItem } from './embrace-data/model'
import { toolOutput } from './embrace-tool-preview'

const kindNames = {
  read: 'read', read_file: 'read', view: 'read',
  edit: 'edit', multiedit: 'edit', apply_patch: 'edit', str_replace: 'edit',
  write: 'write', create_file: 'write',
  grep: 'search', glob: 'search', find: 'search', search: 'search',
  bash: 'run', shell: 'run', exec_command: 'run', shell_command: 'run',
  web_search: 'web', websearch: 'web', fetch: 'web', webfetch: 'web',
  ask: 'question', ask_user: 'question', question: 'question', askuserquestion: 'question', request_user_input: 'question',
  task: 'task', agent: 'task', spawn_agent: 'task',
} as const

export type EmbraceToolKind = typeof kindNames[keyof typeof kindNames] | 'other'

export const embraceToolKind = (name: string): EmbraceToolKind => {
  // Harness-neutral normalization follows the source conversation/tools.ts helper.
  const normalized = name.toLowerCase().split('__').at(-1)?.replace(/^functions\./, '') ?? name.toLowerCase()
  return kindNames[normalized as keyof typeof kindNames] ?? 'other'
}

/** Changes, errors and decisions stay visible in their original position within a tool run. */
export const embraceToolNeedsAttention = (item: Extract<ConversationItem, { _tag: 'ToolCall' }>): boolean => {
  const kind = embraceToolKind(item.name)
  return item.status === 'error' || item.status === 'interrupted' || kind === 'edit' || kind === 'write' || kind === 'question'
    || /^(?:diff --git |--- [^\n]+\n\+\+\+ )/m.test(toolOutput(item.result?.content))
}

