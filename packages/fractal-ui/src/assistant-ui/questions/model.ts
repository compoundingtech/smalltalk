/** Canonical kit question presentation contract. Hosts import these types rather than maintaining copies. */
export type QuestionKind = 'single' | 'multi' | 'text' | 'confirm' | 'permission' | 'plan'
export type Preview =
  | { readonly _tag: 'markdown'; readonly text: string }
  | { readonly _tag: 'command'; readonly command: string; readonly cwd?: string }
  | { readonly _tag: 'diff'; readonly path: string; readonly lines: readonly string[] }
  | { readonly _tag: 'url'; readonly url: string }
export interface OptionView {
  readonly id: string
  readonly label: string
  readonly description?: string
  readonly preview?: Preview
  readonly recommended?: boolean
  readonly tone?: 'default' | 'danger'
  readonly scope?: string
}
export interface QuestionView {
  readonly id: string
  readonly header?: string
  readonly prompt: string
  readonly kind: QuestionKind
  readonly options: readonly OptionView[]
  readonly allowOther: boolean
  readonly allowNote: boolean
  readonly subject?: Preview
  readonly placeholder?: string
  readonly secret?: boolean
  readonly required?: boolean
  readonly allowAttachments?: boolean
}
export interface AnswerDraft {
  readonly selected: readonly string[]
  readonly other?: string
  readonly note?: string
  readonly attachments?: readonly string[]
}
export type Answers = Readonly<Record<string, AnswerDraft>>
export type QuestionState =
  | { readonly _tag: 'pending'; readonly expiresAt?: string }
  | { readonly _tag: 'submitting' }
  | { readonly _tag: 'answered'; readonly answers: Answers; readonly by: string; readonly at: string }
  | { readonly _tag: 'answered-elsewhere'; readonly answers?: Answers; readonly where: string; readonly at: string; readonly draftDiscarded: boolean }
  | { readonly _tag: 'expired'; readonly at: string; readonly autoAnswer?: Answers }
  | { readonly _tag: 'cancelled'; readonly reason: string; readonly at: string }
  | { readonly _tag: 'failed'; readonly error: string }
export interface QuestionRequestView {
  readonly id: string
  readonly source: string
  readonly agentLabel: string
  readonly askedAt: string
  readonly title?: string
  readonly questions: readonly QuestionView[]
  readonly state: QuestionState
  readonly answerable: boolean
  readonly readOnlyReason?: string
  readonly allowChatRedirect: boolean
  readonly declineLabel?: string
}
export type QuestionVariant = 'transcript' | 'pinned' | 'inbox'
export interface QuestionFormProps {
  readonly request: QuestionRequestView
  readonly variant: QuestionVariant
  readonly onSubmit?: (answers: Answers) => void
  readonly onDecline?: () => void
  readonly onChat?: () => void
  readonly onRetry?: () => void
  readonly onOpenTerminal?: () => void
  readonly onJump?: () => void
  readonly draft?: Answers
  readonly onDraftChange?: (answers: Answers) => void
  readonly autoFocus?: boolean
}
