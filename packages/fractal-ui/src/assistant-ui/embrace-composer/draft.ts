import { TokenFieldValue } from 'react-aria-components'
import { Schema } from 'effect'

/** Minimal port of the app composer draft contract; no gateway or resource registry dependency. */
export const MentionToken = Schema.TaggedStruct('Mention', {
  ref: Schema.String, family: Schema.String, label: Schema.String,
}).annotate({ identifier: 'Composer.MentionToken' })
export type MentionToken = typeof MentionToken.Type

export const SlashCommandId = Schema.Literals(['reply', 'title', 'attach', 'terminal'])
export type SlashCommandId = typeof SlashCommandId.Type
export const CommandToken = Schema.TaggedStruct('Command', {
  command: SlashCommandId,
}).annotate({ identifier: 'Composer.CommandToken' })
export type CommandToken = typeof CommandToken.Type
export const DraftToken = Schema.Union([MentionToken, CommandToken])
export type DraftToken = typeof DraftToken.Type
export type Draft = TokenFieldValue<DraftToken>

export interface SlashCommand {
  readonly id: SlashCommandId
  readonly label: string
  readonly description: string
  readonly unavailable?: string
}

/** Only commands with composer-owned serialization are enabled by default. */
export const defaultCommands: readonly SlashCommand[] = [
  { id: 'title', label: '/title', description: 'Use the first line as the message title' },
]

export interface Trigger {
  readonly kind: '@' | '/'
  readonly query: string
  readonly index: number
  readonly start: number
  readonly end: number
}

/** Ported from the real app: commands only at draft start; mentions at word boundaries. */
export const activeTrigger = (draft: Draft): Trigger | undefined => {
  const caret = draft.selectedRange.isCollapsed ? draft.caretPosition : undefined
  const segment = caret === undefined ? undefined : draft.segments[caret.index]
  if (caret === undefined || segment?.type !== 'text') return undefined
  const before = segment.text.slice(0, caret.offset)
  const match = /(^|\s)([@/])([^\s@/]*)$/u.exec(before)
  if (match === null) return undefined
  const kind = match[2] === '@' ? '@' : '/'
  const start = caret.offset - match[3]!.length - 1
  if (kind === '/' && !(caret.index === 0 && before.slice(0, start).trim() === '')) return undefined
  return { kind, query: match[3]!, index: caret.index, start, end: caret.offset }
}

export const insertToken = ({ draft, trigger, token, text }: {
  readonly draft: Draft
  readonly trigger: Trigger
  readonly token: DraftToken
  readonly text: string
}): Draft => draft.replaceRangeWithSegments(
  { index: trigger.index, offset: trigger.start },
  { index: trigger.index, offset: trigger.end },
  [{ type: 'token', text, value: token }, { type: 'text', text: ' ' }],
)

export interface SerializedDraft {
  readonly content: string
  readonly title?: string
  readonly mentions: readonly string[]
  readonly commands: readonly SlashCommandId[]
}

/** App-compatible wire text, deduplicated mentions, and /title semantics. */
export const serializeDraft = (draft: Draft): SerializedDraft => {
  const mentions: string[] = []
  const commands: SlashCommandId[] = []
  let text = ''
  for (const segment of draft.segments) {
    if (segment.type === 'text') text += segment.text
    else if (segment.value?._tag === 'Mention') {
      text += `@${segment.value.ref}`
      if (!mentions.includes(segment.value.ref)) mentions.push(segment.value.ref)
    } else if (segment.value?._tag === 'Command') commands.push(segment.value.command)
  }
  text = text.trim()
  if (!commands.includes('title')) return { content: text, mentions, commands }
  const [first = '', ...rest] = text.split('\n')
  return { content: rest.join('\n').trim() || first, title: first.trim(), mentions, commands }
}

export const draftFromText = (text: string): Draft => {
  const draft = new TokenFieldValue<DraftToken>(text === '' ? [] : [{ type: 'text', text }])
  return draft.withCaretPosition({ index: 0, offset: text.length })
}
