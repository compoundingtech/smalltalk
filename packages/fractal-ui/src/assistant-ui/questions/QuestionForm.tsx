import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { Checkbox, CheckboxGroup, Input, Label, Radio, RadioGroup, Tab, TabList, TabPanel, Tabs, TextField } from 'react-aria-components'
import { surfaceVars as surface, textVars as text, borderVars as border, accentVars as accent, statusVars as status, geometryVars as g, geometryNumbers, radiusVars as r, spaceVars as s, typeVars as t } from '../composition-tokens.stylex'
import { Button, IconButton, Tooltip } from '../composition/Controls'
import { Icon } from '../composition/Icons'
import { Markdown } from '../composition/Markdown'
import { compactTime } from '../sidebar/model'
import type { AnswerDraft, Answers, OptionView, Preview, QuestionFormProps, QuestionState, QuestionView } from './model'

/** The kit's props: the mirrored model's props plus the host clock (`now`, epoch milliseconds) for the "asked" age. */
export type QuestionFormKitProps = QuestionFormProps & { readonly now: number }

/** The automatic free-text row every `allowOther` question receives, worded as the omp ask tool words it. */
export const OTHER_LABEL = 'Other (type your own)'

/** `row` is a radio or checkbox by value (`option:N` or `other`); `other` is the Other text field. */
type FocusTarget = { readonly _tag: 'options' } | { readonly _tag: 'row'; readonly value: string } | { readonly _tag: 'other' }
interface FocusRequest { readonly questionId: string; readonly target: FocusTarget }

const EMPTY_DRAFT: AnswerDraft = { selected: [] }
const OPTIONS_TARGET: FocusTarget = { _tag: 'options' }
const OTHER_VALUE = 'other'
const optionValue = (index: number) => `option:${index}`
const draftFor = (answers: Answers, id: string) => answers[id] ?? EMPTY_DRAFT
const filled = (value: string | undefined) => {
  const trimmed = value?.trim()
  return trimmed === undefined || trimmed === '' ? undefined : trimmed
}
const usesCheckboxes = (question: QuestionView) => question.kind === 'multi'
const clock = new Intl.DateTimeFormat(undefined, { hour: 'numeric', minute: '2-digit' })

/** A question counts as answered once it has a chosen option or non-empty typed text. */
function isQuestionAnswered(question: QuestionView, draft: AnswerDraft): boolean {
  if (filled(draft.other) !== undefined) return true
  return question.kind !== 'text' && draft.selected.length > 0
}

/** The submitted payload: chosen ids, trimmed Other text and trimmed notes; empty text is omitted. */
function settle(questions: readonly QuestionView[], answers: Answers): Answers {
  return Object.fromEntries(questions.map(question => {
    const draft = draftFor(answers, question.id)
    const other = filled(draft.other)
    const note = question.allowNote ? filled(draft.note) : undefined
    const answer: AnswerDraft = {
      selected: question.kind === 'text' ? [] : draft.selected,
      ...(other === undefined ? {} : { other }),
      ...(note === undefined ? {} : { note }),
      ...(draft.attachments === undefined ? {} : { attachments: draft.attachments }),
    }
    return [question.id, answer]
  }))
}

function findFocusTarget(root: HTMLElement | null, request: FocusRequest): HTMLElement | null {
  const panel = root?.querySelector<HTMLElement>(`[data-question-panel="${CSS.escape(request.questionId)}"]`)
  if (panel == null) return null
  const target = request.target
  if (target._tag === 'other') return panel.querySelector<HTMLElement>('[data-question-stop="other"] input')
  const options = panel.querySelector<HTMLElement>('[data-question-stop="options"]')
  if (options === null) return null
  if (target._tag === 'row') return options.querySelector<HTMLElement>(`input[value="${target.value}"]`)
  return options.querySelector<HTMLElement>('input:checked') ?? options.querySelector<HTMLElement>('input')
}

/** Measured form width decides the stacked layout, so a narrow lane stacks even inside a wide window. */
function useStacked(ref: React.RefObject<HTMLElement | null>) {
  const [stacked, setStacked] = React.useState(false)
  React.useLayoutEffect(() => {
    const element = ref.current
    if (element === null) return
    const measure = () => setStacked(element.getBoundingClientRect().width < geometryNumbers.questionStack)
    measure()
    const observer = new ResizeObserver(measure)
    observer.observe(element)
    return () => observer.disconnect()
  }, [ref])
  return stacked
}

function Clock({ at, prefix }: { at: string; prefix: string }) {
  const time = Date.parse(at)
  if (!Number.isFinite(time)) return null
  return <span {...stylex.props(styles.meta)}>{prefix} <time dateTime={at}>{clock.format(time)}</time></span>
}

const STATE_LABEL: Record<QuestionState['_tag'], string> = {
  pending: 'Waiting for you',
  submitting: 'Submitting',
  answered: 'Submitted',
  'answered-elsewhere': 'Answered elsewhere',
  expired: 'Expired',
  cancelled: 'Cancelled',
  failed: 'Not sent',
}

/**
 * Interactive question form for harness asks (parity with the omp ask tool): radio or checkbox options,
 * an automatic Other row, a per-question note, Recommended badges and markdown option previews. Several
 * questions share a tab strip whose header chips mark answered questions; it stacks below
 * `geometryNumbers.questionStack`. Keyboard: arrows move, 1–9 pick, Tab from a question's last control
 * moves to the next question, Enter submits (or moves to the next unanswered question). The host owns
 * the request state, transport and clock (`now`, epoch milliseconds, for the "asked" age); drafts are
 * controlled when `draft` is passed. A question waiting for this user carries the attention hairline and glyph.
 */
export function QuestionForm({ request, variant, onSubmit, onDecline, onChat, onRetry, onOpenTerminal, onJump, draft, onDraftChange, autoFocus = false, now }: QuestionFormKitProps) {
  const rootRef = React.useRef<HTMLElement>(null)
  const stacked = useStacked(rootRef)
  const titleId = React.useId()
  const { questions, state } = request
  const [localDraft, setLocalDraft] = React.useState<Answers>(() => draft ?? {})
  const answers = draft ?? localDraft
  const [chosenId, setActiveId] = React.useState(() => questions[0]?.id)
  const activeId = questions.some(question => question.id === chosenId) ? chosenId : questions[0]?.id
  const [previewIndex, setPreviewIndex] = React.useState<Readonly<Record<string, number>>>({})
  const otherMemory = React.useRef<Record<string, string>>({})
  const focusRequest = React.useRef<FocusRequest | null>(null)

  const interactive = state._tag === 'pending' || state._tag === 'submitting' || state._tag === 'failed'
  const busy = state._tag === 'submitting'
  const canAct = request.answerable && (state._tag === 'pending' || state._tag === 'failed')
  // Pending attention is independent of whether this connection can submit an answer.
  const attention = interactive
  // An unparseable ask time renders no age rather than a guess.
  const askedAt = Date.parse(request.askedAt)
  const askedAge = Number.isFinite(askedAt) ? `${compactTime({ at: askedAt, now })} ago` : undefined
  const answeredCount = questions.filter(question => isQuestionAnswered(question, draftFor(answers, question.id))).length
  const complete = questions.every(question => question.required === false || isQuestionAnswered(question, draftFor(answers, question.id)))

  const requestFocus = (questionId: string, target: FocusTarget) => {
    const request = { questionId, target }
    const element = findFocusTarget(rootRef.current, request)
    // A later request that lands immediately supersedes one still waiting for its panel to mount.
    focusRequest.current = element === null ? request : null
    element?.focus()
  }
  React.useLayoutEffect(() => {
    const pending = focusRequest.current
    if (pending === null) return
    const element = findFocusTarget(rootRef.current, pending)
    if (element === null) return
    focusRequest.current = null
    element.focus()
  })
  React.useEffect(() => {
    const first = questions[0]
    if (autoFocus && canAct && first !== undefined) requestFocus(first.id, OPTIONS_TARGET)
    // Mount-only: autofocus never steals focus on later renders.
  }, [])

  const update = (question: QuestionView, change: (draft: AnswerDraft) => AnswerDraft) => {
    const next = { ...answers, [question.id]: change(draftFor(answers, question.id)) }
    setLocalDraft(next)
    onDraftChange?.(next)
  }
  /** Sets the chosen rows by value; a single-choice question passes one value. Other text survives being switched away from. */
  const choose = (question: QuestionView, values: readonly string[]) => update(question, current => {
    const selected = question.options.filter((_, index) => values.includes(optionValue(index))).map(option => option.id)
    if (values.includes(OTHER_VALUE)) return { ...current, selected, other: current.other ?? otherMemory.current[question.id] ?? '' }
    if (current.other !== undefined) otherMemory.current[question.id] = current.other
    return { ...current, selected, other: undefined }
  })

  /** 1–9 picks the numbered row: options first, then the Other row. Checkbox rows toggle. */
  const pick = (question: QuestionView, index: number) => {
    if (question.kind === 'text') return
    const isOther = question.allowOther && index === question.options.length
    if (!isOther && index >= question.options.length) return
    const value = isOther ? OTHER_VALUE : optionValue(index)
    const multi = usesCheckboxes(question)
    const current = draftFor(answers, question.id)
    const values = [...current.selected.map(id => optionValue(question.options.findIndex(option => option.id === id))), ...(current.other === undefined ? [] : [OTHER_VALUE])]
    const turningOn = !values.includes(value)
    choose(question, !multi ? [value] : turningOn ? [...values, value] : values.filter(entry => entry !== value))
    requestFocus(question.id, isOther && (turningOn || !multi) ? { _tag: 'other' } : { _tag: 'row', value })
  }
  const submit = () => {
    if (!canAct || !complete) return
    onSubmit?.(settle(questions, answers))
  }
  /** Enter submits a complete form; otherwise it moves to the next unanswered required question. */
  const advanceOrSubmit = () => {
    if (complete) return submit()
    const start = questions.findIndex(question => question.id === activeId)
    const ordered = [...questions.slice(start + 1), ...questions.slice(0, start + 1)]
    const next = ordered.find(question => question.required !== false && !isQuestionAnswered(question, draftFor(answers, question.id)))
    if (next === undefined) return
    setActiveId(next.id)
    requestFocus(next.id, OPTIONS_TARGET)
  }
  /** Tab moves through a question's stops (options, Other, note); from the last stop it opens the next question. */
  const tabFrom = (event: React.KeyboardEvent<HTMLElement>, panel: HTMLElement, question: QuestionView) => {
    const stops = [...panel.querySelectorAll<HTMLElement>('[data-question-stop]')]
    const current = (event.target as HTMLElement).closest<HTMLElement>('[data-question-stop]')
    const at = current === null ? -1 : stops.indexOf(current)
    if (at === -1) return
    if (at < stops.length - 1) {
      const next = stops[at + 1]?.querySelector<HTMLElement>('input')
      if (current?.dataset['questionStop'] !== 'options' || next == null) return
      event.preventDefault()
      next.focus()
      return
    }
    const following = questions[questions.indexOf(question) + 1]
    if (following === undefined) return
    event.preventDefault()
    setActiveId(following.id)
    requestFocus(following.id, OPTIONS_TARGET)
  }
  /**
   * Enter is handled in the capture phase: a React Aria Radio or Checkbox row is a pressable label,
   * so a bubbling Enter would pick the focused row before the form saw the key.
   */
  const onKeyDownCapture = (event: React.KeyboardEvent<HTMLElement>) => {
    if (!canAct || event.key !== 'Enter' || !(event.target instanceof HTMLInputElement) || event.altKey || event.ctrlKey || event.metaKey || event.nativeEvent.isComposing) return
    event.preventDefault()
    event.stopPropagation()
    advanceOrSubmit()
  }
  const onKeyDown = (event: React.KeyboardEvent<HTMLElement>) => {
    if (!canAct || event.defaultPrevented || event.altKey || event.ctrlKey || event.metaKey || event.nativeEvent.isComposing) return
    const target = event.target as HTMLElement
    const panel = target.closest<HTMLElement>('[data-question-panel]')
    const question = questions.find(entry => entry.id === (panel?.dataset['questionPanel'] ?? activeId))
    if (question === undefined) return
    if (event.key === 'Tab') {
      if (!event.shiftKey && panel !== null) tabFrom(event, panel, question)
      return
    }
    const typing = target instanceof HTMLInputElement && target.type !== 'checkbox' && target.type !== 'radio'
    if (typing || target instanceof HTMLTextAreaElement) return
    if (/^[1-9]$/.test(event.key)) {
      event.preventDefault()
      pick(question, Number(event.key) - 1)
      return
    }
    if ((event.key === 'ArrowDown' || event.key === 'ArrowUp') && target instanceof HTMLInputElement && target.type === 'checkbox') {
      const boxes = [...(target.closest('[data-question-stop="options"]')?.querySelectorAll<HTMLInputElement>('input[type="checkbox"]') ?? [])]
      const at = boxes.indexOf(target)
      const next = boxes[(at + (event.key === 'ArrowDown' ? 1 : boxes.length - 1)) % boxes.length]
      if (next === undefined) return
      event.preventDefault()
      next.focus()
    }
  }
  const renderQuestion = (question: QuestionView, index: number) => <QuestionPanel
    key={question.id}
    question={question}
    draft={draftFor(answers, question.id)}
    showHeader={questions.length === 1}
    showKeys={canAct}
    stacked={stacked}
    pinned={variant === 'pinned'}
    isDisabled={busy}
    isReadOnly={!request.answerable}
    previewIndex={previewIndex[question.id]}
    promptId={`${titleId}-q${index}`}
    onPreview={index => setPreviewIndex(current => current[question.id] === index ? current : { ...current, [question.id]: index })}
    onChoose={values => choose(question, values)}
    onOther={value => update(question, current => ({ ...current, selected: usesCheckboxes(question) ? current.selected : [], other: value }))}
    onNote={value => update(question, current => ({ ...current, note: value }))}
  />

  const title = request.title ?? (questions.length > 1 ? `${questions.length} questions` : 'Question')
  return <section
    ref={rootRef}
    aria-labelledby={titleId}
    data-question-form=""
    data-state={state._tag}
    data-variant={variant}
    data-stacked={stacked}
    onKeyDownCapture={onKeyDownCapture}
    onKeyDown={onKeyDown}
    {...stylex.props(styles.form, variant === 'pinned' ? styles.pinned : styles.card, variant !== 'pinned' && attention && styles.cardAttention)}
  >
    <div {...stylex.props(styles.header)}>
      <span data-question-glyph="" data-question-attention={attention} {...stylex.props(styles.glyph, attention && styles.glyphAttention)}><Icon name="message" /></span>
      <span id={titleId} {...stylex.props(styles.title)}>{title}</span>
      <span data-question-asked="" {...stylex.props(styles.meta)}>{request.agentLabel}{askedAge !== undefined && <> · asked <time dateTime={request.askedAt}>{askedAge}</time></>}</span>
      <div {...stylex.props(styles.headerEnd)}>
        {state._tag === 'pending' && state.expiresAt !== undefined && <Clock at={state.expiresAt} prefix="Expires" />}
        {(state._tag === 'answered' || state._tag === 'answered-elsewhere' || state._tag === 'expired' || state._tag === 'cancelled') && <Clock at={state.at} prefix={state._tag === 'expired' ? 'Expired at' : 'At'} />}
        <span data-question-state-chip="" {...stylex.props(styles.chip)}>{busy && <Icon name="spinner" spinning size={12} />}{interactive && !request.answerable ? 'Read only' : STATE_LABEL[state._tag]}</span>
        {onOpenTerminal !== undefined && <Tooltip label="Open the agent's terminal"><IconButton icon="drawer" label="Open the agent's terminal" onPress={onOpenTerminal} /></Tooltip>}
        {onJump !== undefined && <Tooltip label="Jump to the conversation"><IconButton icon="chevron-right" label="Jump to the conversation" onPress={onJump} /></Tooltip>}
      </div>
    </div>
    {interactive ? <>
      {/* Tabs mount only with a defined key, so React Aria never flips between controlled and uncontrolled; RAC 1.21.1 types `selectedKey?: Key` reject null. */}
      {questions.length > 1 && activeId !== undefined ? <Tabs
        selectedKey={activeId}
        onSelectionChange={key => setActiveId(String(key))}
        orientation={stacked ? 'vertical' : 'horizontal'}
        {...stylex.props(styles.tabs)}
      >
        <TabList aria-label="Questions" {...stylex.props(styles.tabList, stacked && styles.tabListStacked)}>
          {questions.map((question, index) => {
            const answered = isQuestionAnswered(question, draftFor(answers, question.id))
            const header = question.header ?? `Question ${index + 1}`
            // An explicit name: the marker is a box, so content-derived names would read "Storage , answered".
            return <Tab key={question.id} id={question.id} aria-label={answered ? `${header}, answered` : header} className={({ isSelected, isFocusVisible }) => stylex.props(styles.tab, stacked && styles.tabStacked, isSelected && styles.tabSelected, isFocusVisible && styles.focusRing).className ?? ''}>
              <span {...stylex.props(styles.tabLabel, stacked && styles.tabLabelStacked)}>{header}</span>
              <span data-question-tab-marker={answered ? 'answered' : 'open'} {...stylex.props(styles.marker, answered && styles.markerAnswered)}>{answered && <Icon name="check" size={12} />}</span>
            </Tab>
          })}
        </TabList>
        {questions.map((question, index) => <TabPanel key={question.id} id={question.id} {...stylex.props(styles.tabPanel)}>{renderQuestion(question, index)}</TabPanel>)}
      </Tabs> : questions.map(renderQuestion)}
      {state._tag === 'failed' && <p role="alert" {...stylex.props(styles.problem)}><span data-question-problem-glyph="" {...stylex.props(styles.problemGlyph)}><Icon name="alert" size={12} /></span>Not sent: {state.error}</p>}
      <div {...stylex.props(styles.footer)}>
        {questions.length > 1 && <span aria-live="polite" {...stylex.props(styles.meta)}>{answeredCount} of {questions.length} answered</span>}
        {canAct ? <span {...stylex.props(styles.hints)}>
          {questions.some(question => question.kind !== 'text') && <span><kbd {...stylex.props(styles.kbd)}>1</kbd>–<kbd {...stylex.props(styles.kbd)}>9</kbd> pick</span>}
          {questions.length > 1 && <span><kbd {...stylex.props(styles.kbd)}>Tab</kbd> next question</span>}
          <span><kbd {...stylex.props(styles.kbd)}>Enter</kbd> submit</span>
        </span> : !request.answerable && request.readOnlyReason !== undefined && <span {...stylex.props(styles.meta)}>{request.readOnlyReason}</span>}
        {request.answerable && <div {...stylex.props(styles.actions)}>
          {onDecline !== undefined && <Button isDisabled={busy} onPress={onDecline}>{request.declineLabel ?? 'Skip'}</Button>}
          {request.allowChatRedirect && onChat !== undefined && <Button isDisabled={busy} onPress={onChat}>Chat about this</Button>}
          {state._tag === 'failed' && onRetry !== undefined
            ? <Button variant="outline" size="md" onPress={onRetry}>Retry</Button>
            : <Button variant="outline" size="md" isDisabled={busy || !complete} onPress={submit}>{busy ? 'Submitting' : 'Submit'}</Button>}
        </div>}
      </div>
    </> : <Settled state={state} questions={questions} />}
  </section>
}

function Settled({ state, questions }: { state: Exclude<QuestionState, { _tag: 'pending' | 'submitting' | 'failed' }>; questions: readonly QuestionView[] }) {
  const line = (() => {
    switch (state._tag) {
      case 'answered': return `Submitted by ${state.by}.`
      case 'answered-elsewhere': return `Answered in ${state.where}.${state.draftDiscarded ? ' Your draft here was discarded.' : ''}`
      case 'expired': return state.autoAnswer === undefined ? 'Expired before an answer was submitted.' : 'Expired; the agent continued with the automatic answers below.'
      case 'cancelled': return `Cancelled: ${state.reason}`
    }
  })()
  const answers = state._tag === 'answered' ? state.answers : state._tag === 'answered-elsewhere' ? state.answers : state._tag === 'expired' ? state.autoAnswer : undefined
  return <>
    <p data-question-settled="" {...stylex.props(styles.settledLine)}>{line}</p>
    <dl {...stylex.props(styles.summary)}>
      {questions.map(question => {
        const answer = answers?.[question.id]
        const note = filled(answer?.note)
        return <div key={question.id} data-question-summary={question.id} {...stylex.props(styles.summaryRow)}>
          <dt {...stylex.props(styles.summaryPrompt)}>{question.header !== undefined && <span {...stylex.props(styles.chip)}>{question.header}</span>}{question.prompt}</dt>
          <dd {...stylex.props(styles.summaryAnswer)}>{answerText(question, answer)}</dd>
          {note !== undefined && <dd {...stylex.props(styles.summaryNote)}>Note: {note}</dd>}
        </div>
      })}
    </dl>
  </>
}

function answerText(question: QuestionView, answer: AnswerDraft | undefined): string {
  const labels = answer?.selected.flatMap(id => question.options.find(option => option.id === id)?.label ?? []) ?? []
  const typed = filled(answer?.other)
  // Typed text of a secret question stays masked once settled, as it was while typing.
  const parts = typed === undefined ? labels : [...labels, question.secret === true ? 'Answer hidden' : question.kind === 'text' ? typed : `Other: ${typed}`]
  return parts.length === 0 ? 'No answer' : parts.join(', ')
}

interface QuestionPanelProps {
  readonly question: QuestionView
  readonly draft: AnswerDraft
  readonly showHeader: boolean
  /** Digit keycaps show only while 1–9 can act. */
  readonly showKeys: boolean
  readonly stacked: boolean
  readonly pinned: boolean
  readonly isDisabled: boolean
  readonly isReadOnly: boolean
  readonly previewIndex: number | undefined
  readonly promptId: string
  readonly onPreview: (index: number) => void
  readonly onChoose: (values: readonly string[]) => void
  readonly onOther: (value: string) => void
  readonly onNote: (value: string) => void
}

function QuestionPanel({ question, draft, showHeader, showKeys, stacked, pinned, isDisabled, isReadOnly, previewIndex, promptId, onPreview, onChoose, onOther, onNote }: QuestionPanelProps) {
  const hasPreview = question.options.some(option => option.preview !== undefined)
  const [promptExpanded, setPromptExpanded] = React.useState(false)
  const [promptOverflows, setPromptOverflows] = React.useState(false)
  const promptRef = React.useRef<HTMLSpanElement>(null)
  const promptClamped = pinned && !promptExpanded
  // The disclosure exists only while the two-line clamp hides text: measured after layout and again on every resize.
  React.useLayoutEffect(() => {
    const element = promptRef.current
    if (!promptClamped || element === null) return
    const measure = () => setPromptOverflows(element.scrollHeight > element.clientHeight)
    measure()
    const observer = new ResizeObserver(measure)
    observer.observe(element)
    return () => observer.disconnect()
  }, [promptClamped, question.prompt])
  const selectedIndexes = draft.selected.map(id => question.options.findIndex(option => option.id === id)).filter(index => index >= 0)
  const fallbackIndex = selectedIndexes[0] ?? Math.max(0, question.options.findIndex(option => option.recommended === true))
  const preview = question.options[previewIndex ?? fallbackIndex]
  // A text question's own field holds its answer, so only option questions open a separate Other field.
  const otherOpen = question.kind !== 'text' && question.allowOther && draft.other !== undefined
  const multi = usesCheckboxes(question)
  const inputType = question.secret === true ? 'password' : 'text'
  // A description describes its row rather than naming it, so it is referenced by id and hidden from the name.
  const rows = question.options.map((option, index) => ({ option, index, value: optionValue(index), descriptionId: option.description === undefined ? undefined : `${promptId}-d${index}` }))
  const previewCaptionId = `${promptId}-preview`
  /** Hovering or focusing an option row previews it; the Other row has no preview. */
  const previewRow = (event: React.SyntheticEvent<HTMLElement>) => {
    const index = Number((event.target as HTMLElement).closest('label')?.querySelector<HTMLElement>('[data-option-index]')?.dataset['optionIndex'])
    if (Number.isInteger(index) && index < question.options.length) onPreview(index)
  }
  const rowClass = ({ isSelected, isFocusVisible }: { isSelected: boolean; isFocusVisible: boolean }) => stylex.props(styles.option, isSelected && styles.optionSelected, isFocusVisible && styles.focusRing, (isReadOnly || isDisabled) && styles.optionInert).className ?? ''

  const control = question.kind === 'text'
    ? <TextField aria-labelledby={promptId} value={draft.other ?? ''} onChange={onOther} isDisabled={isDisabled} isReadOnly={isReadOnly}>
        <Input type={inputType} placeholder={question.placeholder ?? 'Type your answer'} {...stylex.props(styles.input)} />
      </TextField>
    : multi
      ? <CheckboxGroup aria-labelledby={promptId} value={[...selectedIndexes.map(optionValue), ...(otherOpen ? [OTHER_VALUE] : [])]} onChange={onChoose} isDisabled={isDisabled} isReadOnly={isReadOnly} {...stylex.props(styles.options)}>
          {rows.map(row => <Checkbox key={row.option.id} value={row.value} aria-describedby={row.descriptionId} className={rowClass}>{({ isSelected }) => <OptionContent option={row.option} index={row.index} descriptionId={row.descriptionId} showKey={showKeys} isSelected={isSelected} multi />}</Checkbox>)}
          {question.allowOther && <Checkbox value={OTHER_VALUE} className={rowClass}>{({ isSelected }) => <OtherContent index={question.options.length} showKey={showKeys} isSelected={isSelected} multi />}</Checkbox>}
        </CheckboxGroup>
      : <RadioGroup aria-labelledby={promptId} value={otherOpen ? OTHER_VALUE : selectedIndexes[0] === undefined ? null : optionValue(selectedIndexes[0])} onChange={value => onChoose([value])} isDisabled={isDisabled} isReadOnly={isReadOnly} {...stylex.props(styles.options)}>
          {rows.map(row => <Radio key={row.option.id} value={row.value} aria-describedby={row.descriptionId} className={rowClass}>{({ isSelected }) => <OptionContent option={row.option} index={row.index} descriptionId={row.descriptionId} showKey={showKeys} isSelected={isSelected} multi={false} />}</Radio>)}
          {question.allowOther && <Radio value={OTHER_VALUE} className={rowClass}>{({ isSelected }) => <OtherContent index={question.options.length} showKey={showKeys} isSelected={isSelected} multi={false} />}</Radio>}
        </RadioGroup>

  return <div data-question-panel={question.id} {...stylex.props(styles.question)}>
    <div {...stylex.props(styles.promptRow)}>
      {showHeader && question.header !== undefined && <span {...stylex.props(styles.chip)}>{question.header}</span>}
      <span ref={promptRef} id={promptId} {...stylex.props(styles.prompt, promptClamped && styles.promptClamped)}>{question.prompt}</span>
      {pinned && (promptExpanded || promptOverflows) && <Button data-question-prompt-disclosure="" variant="ghost" size="sm" aria-expanded={promptExpanded} aria-controls={promptId} onPress={() => setPromptExpanded(expanded => !expanded)}>{promptExpanded ? 'Show less' : 'Show full question'}</Button>}
    </div>
    {question.subject !== undefined && <div {...stylex.props(styles.subject)}><PreviewBody preview={question.subject} /></div>}
    <div {...stylex.props(styles.body, hasPreview && !stacked && styles.bodyBeside)}>
      <div {...stylex.props(styles.column)}>
        <div data-question-stop="options" onPointerOver={previewRow} onFocus={previewRow}>{control}</div>
        {otherOpen && <div data-question-stop="other" {...stylex.props(styles.otherField)}>
          <TextField aria-label={`Other answer: ${question.header ?? question.prompt}`} value={draft.other ?? ''} onChange={onOther} isDisabled={isDisabled} isReadOnly={isReadOnly}>
            <Input type={inputType} placeholder={question.placeholder ?? 'Type your own answer'} {...stylex.props(styles.input)} />
          </TextField>
        </div>}
      </div>
      {hasPreview && preview !== undefined && <figure data-question-preview={preview.id} aria-labelledby={previewCaptionId} {...stylex.props(styles.preview)}>
        <figcaption id={previewCaptionId} {...stylex.props(styles.meta)}>Preview · {preview.label}</figcaption>
        <PreviewBody preview={preview.preview} />
      </figure>}
    </div>
    {question.allowNote && !isReadOnly && <div data-question-stop="note">
      <TextField value={draft.note ?? ''} onChange={onNote} isDisabled={isDisabled} isReadOnly={isReadOnly} {...stylex.props(styles.note)}>
        <Label {...stylex.props(styles.meta)}>Note</Label>
        <Input placeholder="Add a note for the agent (optional)" {...stylex.props(styles.input)} />
      </TextField>
    </div>}
  </div>
}

const PREVIEW_KIND: Record<Exclude<Preview['_tag'], 'markdown'>, string> = { command: 'Command', diff: 'Diff', url: 'Link' }
function PreviewBody({ preview }: { preview: Preview | undefined }) {
  if (preview === undefined) return <p {...stylex.props(styles.previewEmpty)}>No preview for this option.</p>
  if (preview._tag === 'markdown') return <Markdown text={preview.text} />
  return <p {...stylex.props(styles.previewEmpty)}>{PREVIEW_KIND[preview._tag]} previews are not shown in this form yet.</p>
}

function Indicator({ isSelected, multi }: { isSelected: boolean; multi: boolean }) {
  return <span aria-hidden="true" {...stylex.props(styles.indicator, multi ? styles.box : styles.ring, isSelected && styles.indicatorSelected)}>
    {isSelected && (multi ? <Icon name="check" size={12} /> : <span {...stylex.props(styles.dot)} />)}
  </span>
}

function OptionContent({ option, index, descriptionId, showKey, isSelected, multi }: { option: OptionView; index: number; descriptionId: string | undefined; showKey: boolean; isSelected: boolean; multi: boolean }) {
  return <div data-option-index={index} {...stylex.props(styles.optionRow)}>
    <Indicator isSelected={isSelected} multi={multi} />
    <span {...stylex.props(styles.optionText)}>
      <span {...stylex.props(styles.optionLabel)}>
        {option.label}
        {option.recommended === true && <span data-recommended="" {...stylex.props(styles.badge)}>Recommended</span>}
        {option.tone === 'danger' && <span {...stylex.props(styles.danger)}><Icon name="alert" size={12} label="Destructive" /></span>}
      </span>
      {option.description !== undefined && <span id={descriptionId} aria-hidden="true" {...stylex.props(styles.description)}>{option.description}</span>}
    </span>
    {showKey && index < 9 && <span data-question-keycap="" aria-hidden="true" {...stylex.props(styles.keycap)}>{index + 1}</span>}
  </div>
}

function OtherContent({ index, showKey, isSelected, multi }: { index: number; showKey: boolean; isSelected: boolean; multi: boolean }) {
  return <div data-option-index={index} {...stylex.props(styles.optionRow)}>
    <Indicator isSelected={isSelected} multi={multi} />
    <span {...stylex.props(styles.optionText)}><span {...stylex.props(styles.optionLabel)}>{OTHER_LABEL}</span></span>
    {showKey && index < 9 && <span data-question-keycap="" aria-hidden="true" {...stylex.props(styles.keycap)}>{index + 1}</span>}
  </div>
}

const styles = stylex.create({
  form: { display: 'flex', flexDirection: 'column', gap: s.lg, width: '100%', minWidth: 0, boxSizing: 'border-box', color: text.fg, fontFamily: t.fontSans, fontSize: t.uiSize, lineHeight: t.uiLeading },
  card: { paddingBlock: s.md, paddingInline: s.lg, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.md, backgroundColor: surface.raised },
  /** E37: a question waiting on this user carries the attention hairline. */
  cardAttention: { borderColor: status.attention },
  pinned: { paddingBlock: s.md, paddingInline: s.zero, backgroundColor: surface.transparent },
  header: { display: 'flex', alignItems: 'center', flexWrap: 'wrap', gap: s.sm, minWidth: 0 },
  glyph: { display: 'inline-flex', color: text.fgMuted },
  glyphAttention: { color: status.attention },
  title: { fontWeight: t.weightMedium, color: text.fg },
  meta: { color: text.fgMuted, fontSize: t.metaSize, lineHeight: t.metaLeading },
  headerEnd: { display: 'flex', alignItems: 'center', gap: s.sm, marginInlineStart: 'auto' },
  chip: { display: 'inline-flex', alignItems: 'center', gap: s.xs, flexShrink: 0, paddingInline: s.sm, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.full, color: text.fgSoft, fontSize: t.metaSize, lineHeight: t.metaLeading, marginInlineEnd: s.sm },
  tabs: { display: 'flex', flexDirection: 'column', gap: s.md, minWidth: 0 },
  tabList: { display: 'flex', flexWrap: 'wrap', gap: s.md },
  tabListStacked: { flexDirection: 'column', flexWrap: 'nowrap' },
  tab: { display: 'inline-flex', alignItems: 'center', gap: s.xs, minWidth: 0, height: g.controlMd, paddingInline: s.md, boxSizing: 'border-box', borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.full, color: text.fgMuted, fontSize: t.metaSize, lineHeight: t.metaLeading, cursor: 'pointer', outlineStyle: 'none', ':hover': { color: text.fg } },
  tabStacked: { width: '100%', justifyContent: 'space-between', borderRadius: r.control },
  tabSelected: { color: text.fg, backgroundColor: surface.rowActive },
  tabLabel: { maxWidth: g.tabMax, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' },
  tabLabelStacked: { maxWidth: 'none' },
  marker: { display: 'inline-flex', alignItems: 'center', justifyContent: 'center', width: g.iconSm, height: g.iconSm, flexShrink: 0 },
  markerAnswered: { color: text.fgSoft },
  tabPanel: { outlineStyle: 'none', minWidth: 0 },
  focusRing: { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary, outlineOffset: g.focusOffset },
  question: { display: 'flex', flexDirection: 'column', gap: s.md, minWidth: 0 },
  promptRow: { display: 'flex', alignItems: 'baseline', flexWrap: 'wrap', minWidth: 0 },
  prompt: { fontWeight: t.weightMedium, color: text.fg, overflowWrap: 'anywhere' },
  promptClamped: { display: '-webkit-box', WebkitBoxOrient: 'vertical', WebkitLineClamp: 2, overflow: 'hidden' },
  subject: { minWidth: 0, color: text.fgSoft },
  body: { display: 'flex', flexDirection: 'column', gap: s.md, minWidth: 0 },
  bodyBeside: { display: 'grid', gridTemplateColumns: 'minmax(0, 1fr) minmax(0, 1fr)', alignItems: 'start', gap: s.lg },
  column: { display: 'flex', flexDirection: 'column', gap: s.sm, minWidth: 0 },
  options: { display: 'flex', flexDirection: 'column', gap: s.xs2 },
  option: { display: 'block', borderRadius: r.control, cursor: 'pointer', outlineStyle: 'none', ':hover': { backgroundColor: surface.washSubtle } },
  /** rowHover, not rowActive: muted descriptions keep AA contrast on the selected row in both schemes. */
  optionSelected: { backgroundColor: surface.rowHover, ':hover': { backgroundColor: surface.rowHover } },
  optionInert: { cursor: 'default' },
  optionRow: { display: 'flex', alignItems: 'flex-start', gap: s.md, paddingBlock: s.sm, paddingInline: s.md, minWidth: 0 },
  indicator: { display: 'inline-flex', alignItems: 'center', justifyContent: 'center', width: g.icon, height: g.icon, marginBlockStart: s.xs2, flexShrink: 0, boxSizing: 'border-box', borderWidth: g.hairline, borderStyle: 'solid', borderColor: text.fgMuted, color: text.fg },
  ring: { borderRadius: r.full },
  box: { borderRadius: r.keyboard },
  indicatorSelected: { borderColor: text.fg },
  dot: { width: g.statusDot, height: g.statusDot, borderRadius: r.full, backgroundColor: text.fg },
  optionText: { display: 'flex', flexDirection: 'column', flexGrow: 1, minWidth: 0 },
  optionLabel: { display: 'flex', alignItems: 'center', flexWrap: 'wrap', gap: s.sm, color: text.fg, overflowWrap: 'anywhere' },
  description: { color: text.fgMuted, fontSize: t.metaSize, lineHeight: t.metaLeading },
  badge: { display: 'inline-flex', alignItems: 'center', paddingInline: s.sm, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.full, color: text.fgSoft, fontSize: t.denseSize, lineHeight: t.metaLeading },
  danger: { display: 'inline-flex', color: text.fgSoft },
  keycap: { flexShrink: 0, minWidth: g.iconSm, paddingInline: s.xs, textAlign: 'center', borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.keyboard, color: text.fgMuted, fontFamily: t.fontMono, fontSize: t.denseSize, lineHeight: t.metaLeading },
  otherField: { paddingInlineStart: s.panel },
  input: { width: '100%', height: g.controlLg, boxSizing: 'border-box', paddingInline: s.md, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.control, backgroundColor: surface.controlFill, color: text.fg, fontFamily: t.fontSans, fontSize: t.uiSize, outlineStyle: 'none', '::placeholder': { color: text.fgMuted }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary, outlineOffset: g.focusOffset } },
  note: { display: 'flex', flexDirection: 'column', gap: s.xs },
  preview: { margin: s.zero, display: 'flex', flexDirection: 'column', gap: s.sm, minWidth: 0, padding: s.md, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.md, backgroundColor: surface.washSubtle },
  previewEmpty: { margin: s.zero, color: text.fgMuted, fontSize: t.metaSize, lineHeight: t.metaLeading },
  problem: { display: 'flex', alignItems: 'center', gap: s.xs, margin: s.zero, color: text.fgSoft, fontSize: t.metaSize, lineHeight: t.metaLeading },
  problemGlyph: { display: 'inline-flex', color: status.dangerFg },
  footer: { display: 'flex', alignItems: 'center', flexWrap: 'wrap', gap: s.md, minWidth: 0 },
  hints: { display: 'flex', flexWrap: 'wrap', gap: s.md, color: text.fgMuted, fontSize: t.metaSize, lineHeight: t.metaLeading },
  kbd: { paddingInline: s.xs, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.keyboard, color: text.fgSoft, fontFamily: t.fontMono, fontSize: t.denseSize },
  actions: { display: 'flex', alignItems: 'center', gap: s.sm, marginInlineStart: 'auto' },
  settledLine: { margin: s.zero, color: text.fgSoft },
  summary: { display: 'flex', flexDirection: 'column', gap: s.md, margin: s.zero },
  summaryRow: { display: 'flex', flexDirection: 'column', gap: s.xs2, minWidth: 0 },
  summaryPrompt: { display: 'flex', alignItems: 'baseline', flexWrap: 'wrap', color: text.fgMuted, fontSize: t.metaSize, lineHeight: t.metaLeading, overflowWrap: 'anywhere' },
  summaryAnswer: { margin: s.zero, color: text.fg, overflowWrap: 'anywhere' },
  summaryNote: { margin: s.zero, color: text.fgSoft, fontSize: t.metaSize, lineHeight: t.metaLeading, overflowWrap: 'anywhere' },
})
